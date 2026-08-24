//! A [`CallResolver`] backed by a compiled assembly's metadata.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use lamella_cil::{Opcode, Operand};
use lamella_ir::{Function, MirType, StaticOwner, TypeHandle};
use lamella_metadata::tables::table;
use lamella_metadata::{
    Assembly, CharSet, Method, MethodKind, MethodSig, ResolvedMethod, SigType, TargetLayout,
    TypeDef, TypeLayout, TypeName, exception_tag_for_name, fnv1a32, layout_value_type,
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

/// The MIR slot for an instantiation of a VALUE type -- `Holder<int>` -- laid out under its
/// arguments, or `None` when it cannot be laid out exactly.
///
/// # Why this is a slot type at all, and what it depends on
///
/// The MIR type needs a SIZE and a GC TRACE MAP, and both come from the definition's fields
/// SUBSTITUTED with the arguments. A slot that cannot carry a trace map has nothing to answer with
/// and must REFUSE instead: `Holder<string>`'s reference field would otherwise be laid in a cell the
/// collector never visits. So the ability to describe the cell is the precondition for this shape
/// being a slot type at all, and the refusal remains the honest answer wherever it is missing.
///
/// **THE IDENTITY IS THE CANONICAL SPELLING, NOT A ROW.** `Holder<int>` and `Holder<string>` are two
/// types with two layouts and two trace maps, and a handle taken from the DEFINITION's token would
/// make them one -- a collapse arriving through the layout rather than through the type tag, and no
/// less wrong for it. [`crate::generics::instantiation_handle`] derives the handle from the name, so
/// two builds of one instantiation agree by construction.
///
/// `None` (and therefore a refusal at the caller) when the definition does not resolve, a field does
/// not substitute, the layout does not compute, or the trace map does not fit -- every one of which
/// is a cell this tier cannot describe exactly, and an inexact cell is a wrong GC map rather than a
/// wrong size.
pub(crate) fn instantiated_value_type_slot<'x>(
    sig: &SigType,
    assembly: &'x Assembly<'x>,
    references: &[&'x Assembly<'x>],
    target: &TargetLayout,
) -> Option<MirType> {
    let SigType::GenericInst {
        definition,
        arguments,
    } = sig
    else {
        return None;
    };
    let SigType::ValueType(token) = definition.as_ref() else {
        return None;
    };
    let (owner, type_def) = if token.table() == table::TYPE_DEF {
        (assembly, assembly.type_def(token.row())?)
    } else {
        let (namespace, name) = assembly.type_token_full_name(*token)?;
        let (ordinal, type_def) = Assembly::find_in_references(references, &namespace, &name)?;
        (*references.get(ordinal)?, type_def)
    };
    let mut fields = Vec::new();
    for field in type_def.fields().filter(|field| !field.is_static()) {
        fields.push(crate::generics::substitute_sig(&field.signature()?, arguments)?);
    }
    let layout = layout_value_type(&fields, target, &|token| {
        owner.value_type_layout(token, target).ok()
    })
    .ok()?;
    Some(MirType::ValueType {
        handle: crate::generics::instantiation_handle(&crate::generics::spell_sig(assembly, sig)?),
        size: layout.size,
        refs: ref_words_of(&layout.reference_offsets)?,
    })
}

/// A value type's GC trace map from the byte offsets its layout already computed, or `None` when it
/// does not fit [`lamella_ir::RefWords`]' 32-word bound.
///
/// One function, because `build::mir_type` and `resolver::mir_type` are twins that must type one
/// struct identically -- the pair has drifted before, and a slot they disagree about is a verify
/// mismatch at best and an unenumerated root at worst.
pub(crate) fn ref_words_of(reference_offsets: &[u32]) -> Option<lamella_ir::RefWords> {
    lamella_ir::RefWords::from_offsets(reference_offsets)
}

/// One type identity, as minted while reading a REFERENCE's own metadata, respelled the way the
/// CALLER spells that reference -- or `None` when it has no caller-side spelling at all.
///
/// **THIS IS THE THIRD OF THE THREE CORRECTIONS A REBASED BODY NEEDS** (the other two are its calls
/// out and its statics, both in [`MetadataResolver`]), and it is the one that cannot be made where
/// the identity is minted: a handle is produced at a dozen sites -- an `Alloc`, a cast target, an
/// array's element, a delegate's layout, a box target -- and a rule threaded through all of them
/// gains a new site that silently keeps the old answer. Here the rule is TOTAL over the handle
/// encoding, so a byte with no arm is a refusal rather than a pass-through.
///
/// - A this-assembly `TypeDef` (`0x02`) is the owner's own row, and the caller names it by the
///   reference-owned encoding -- the ordinal it holds the owner at, plus that row.
/// - An already-reference-owned handle (`0x03`) needs NOTHING. Its ordinal indexes the owner's own
///   reference list, and a rebased resolver is built over `caller.references[..ordinal]`, so the
///   two lists agree on every entry the owner can name.
/// - A generic INSTANTIATION (`0x09`) is derived from its canonical SPELLING rather than a row, so
///   it is already assembly-independent -- that is the property that lets one descriptor serve one
///   instantiation across the link.
/// - A synthesized array (`0x04`) carries an element KIND, not a row.
/// - An ARRAY of any of those is its element's identity lifted by a fixed offset, so it rebases by
///   rebasing the element and lifting again.
/// - An unresolved `TypeRef` (`0x01`) is refused: it names a type the OWNER could not resolve, and
///   the caller cannot resolve it either without asking a different question than the one the owner
///   asked. Anything else -- a bare `TypeSpec` (`0x1B`), a byte with no meaning -- is refused for
///   the same reason: there is no caller-side spelling to give it.
#[must_use]
pub(crate) fn rebased_handle(handle: TypeHandle, ordinal: u8) -> Option<TypeHandle> {
    let table = handle.0 >> 24;
    let type_def = u32::from(table::TYPE_DEF);
    if table == type_def {
        return Some(reference_handle(ordinal as usize, handle.0));
    }
    if table == REFERENCE_HANDLE_TABLE
        || table == lamella_ir::SYNTHETIC_ARRAY_HANDLE_TABLE
        || table == crate::generics::INSTANTIATION_HANDLE_TABLE
    {
        return Some(handle);
    }
    let lifted = table.checked_sub(lamella_ir::ARRAY_HANDLE_TABLE_OFFSET);
    if lifted.is_some_and(|element_table| {
        element_table == type_def
            || element_table == REFERENCE_HANDLE_TABLE
            || element_table == crate::generics::INSTANTIATION_HANDLE_TABLE
    }) {
        let element = TypeHandle(handle.0 - (lamella_ir::ARRAY_HANDLE_TABLE_OFFSET << 24));
        return Some(lamella_ir::array_handle(rebased_handle(element, ordinal)?));
    }
    None
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
    } else if handle >> 24 == crate::generics::INSTANTIATION_HANDLE_TABLE {
        alloc::format!("{}{}", lamella_elf::TYPE_DESC_PREFIX, handle)
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
///
/// `Clone` so one monomorphized body can be lowered under its own instantiation without rebuilding
/// the references, the rid map and the box-target set -- see [`Self::with_type_arguments`].
#[derive(Clone)]
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
    /// The type arguments in force while lowering ONE monomorphized body -- `[I4]` while lowering
    /// `Box<int>`'s copy of `Box`1`'s methods. Empty for every ordinary body, which is what keeps
    /// the non-generic path byte-identical.
    type_arguments: Vec<SigType>,
    /// The SAME arguments, re-expressed by [`caller_resolved_arguments`] so that reading them does
    /// not depend on which assembly reads them -- what a LAYOUT substitutes, where
    /// [`Self::type_arguments`] is what an IDENTITY substitutes.
    ///
    /// **TWO LISTS BECAUSE ONE LIST ANSWERS TWO QUESTIONS AND THEY WANT DIFFERENT ANSWERS.** A
    /// caller's `MyEnum` is its underlying integer for a SIZE and is emphatically not
    /// `System.Int32` for a NAME. Equal to `type_arguments` for every ordinary body and for every
    /// argument that names nothing, which is what keeps the non-generic path byte-identical.
    layout_arguments: Vec<SigType>,
    /// The assembly the type ARGUMENTS are written in, when it is not [`Self::assembly`] -- set only
    /// by [`Self::rebased_on_reference`], where `assembly` becomes the definition's OWNER while the
    /// arguments remain the CALLER's. `None` everywhere else, which is every ordinary resolver.
    argument_assembly: Option<&'a Assembly<'a>>,
    /// The METHOD type arguments in force while lowering a generic method's monomorphized body --
    /// what `!!n` substitutes to. Separate from `type_arguments` because the two axes are separate:
    /// `Pick<int>` called inside `Box<string>` resolves `!!0` to `int` and `!0` to `string`, and one
    /// list would answer the same for both.
    method_arguments: Vec<SigType>,
    /// The monomorphized bodies this module emits, keyed by `(instantiation, method, parameters)`.
    ///
    /// **THE RESOLVER MUST ALREADY HOLD THIS WHEN IT ANSWERS A CALL**, because the reachability
    /// walk discovers a monomorphized body the ordinary way -- through the `Inst::Call` this map
    /// produces. Empty for every ordinary build, and an empty plan answers nothing, which is what
    /// keeps the non-generic path byte-identical.
    mono: crate::generics::MonoPlan,
    /// Every type token this assembly's own code takes a `box` of, deduplicated -- the bound on
    /// [`Self::unbox_accepted_handles`].
    ///
    /// **THIS EXISTS BECAUSE THE UNBOUNDED SET COSTS 21 KB AN IMAGE.** An unbox's accepted set has
    /// to admit the enum/underlying interchange, and naming every type that could interchange drags
    /// each one's DESCRIPTOR -- and a descriptor drags its whole VTABLE (the mechanism the generics
    /// spike measured: a `long`+`double` step cost +25,272 B of which only +8,020 was specializations).
    /// Enumerating ~20 candidates per unbox site grew every image 19%, `hello` included, because
    /// corlib's own bodies unbox. A box target already has a descriptor -- boxing it laid one -- so
    /// bounding the set this way costs compares instead of vtables.
    box_target_tokens: Vec<Token>,
    /// The initialization thunk of each type demanding precise timing, as `(TypeDef row, function
    /// index)`. Empty for an eager build, and empty answers nothing -- which is what keeps every
    /// image with no such type byte-identical.
    ///
    /// **THIS HAS TO BE ATTACHED BEFORE ANY BODY IS LOWERED**, for the reason
    /// [`Self::with_monomorphized`] gives at greater length: a trigger site emits a `Call` to this
    /// index, and a resolver handed the map late emits no call, leaving a site that reads the
    /// uninitialized field and answers from zeroed storage.
    type_init_thunks: Vec<(u32, u32)>,
    /// Set while this resolver is REBASED: `assembly` is a REFERENCE of the module being emitted,
    /// not the module itself. See [`ReferenceOwner`].
    reference_owner: Option<ReferenceOwner>,
}

/// The CALLER's view of the assembly a REBASED resolver reads -- what a body lowered from a
/// referenced assembly's own CIL needs in order to be spelled the way the emitting module spells
/// that assembly.
///
/// # Why a resolver is rebased at all
///
/// A cross-assembly monomorphized body lowers the OWNER's CIL into the CALLER's function table.
/// Every token in that CIL -- a field, a call, a string literal, a static -- is the owner's, so the
/// resolver reading it has to be over the OWNER. What it then MINTS, though, lands in the caller's
/// object: an own-assembly call would name a function index the caller does not have, an own-assembly
/// static would address the caller's region, and an own-assembly type identity would name whichever
/// of the caller's types happens to share that row. Each of those is a link that succeeds and a
/// program that answers wrong, which is why the three are corrected here rather than left to the
/// emitter to notice.
///
/// **THE ORDINAL IS THE CALLER'S, AND THE REFERENCES ARE THE OWNER'S OWN PREFIX.** A rebased
/// resolver is handed `caller.references[..ordinal]` -- the assemblies the owner itself was built
/// against, in the layering the reference list already encodes -- so an identity the owner resolves
/// through ITS references gets an ordinal that means the same assembly in the caller's list, and
/// needs no correction at all.
#[derive(Clone)]
pub(crate) struct ReferenceOwner {
    /// The owner's ordinal in the CALLER's reference list.
    ordinal: u8,
    /// The owner's own per-function symbol names, `MethodDef`-rid indexed -- exactly the names its
    /// own library object defines (`build::library_symbol_names`), so a call OUT of a rebased body
    /// names a symbol that exists rather than one derived a second way.
    ///
    /// `None` at a rid whose body that object does NOT carry -- an open generic definition's
    /// methods, which its own lowering skips and leaves as a `stub()` that RETURNS. Naming one
    /// would be a call that links and answers zero.
    symbols: Vec<Option<alloc::string::String>>,
}

impl ReferenceOwner {
    /// The caller-side view of an owner at `ordinal` whose own object defines `symbols`.
    pub(crate) fn new(ordinal: u8, symbols: Vec<Option<alloc::string::String>>) -> ReferenceOwner {
        ReferenceOwner { ordinal, symbols }
    }
}

/// Every distinct type token `assembly`'s method bodies take a `box` of. A boxed value cannot exist
/// unless some `box` created it, so this is the population an `unbox` in the same image can meet.
fn box_target_tokens(assembly: &Assembly) -> Vec<Token> {
    let mut tokens = Vec::new();
    for type_def in assembly.type_defs() {
        for method in type_def.methods() {
            let Some(body) = method.body() else {
                continue;
            };
            for inst in body.code.iter() {
                if inst.opcode == Opcode::Box {
                    if let Operand::Token(token) = inst.operand {
                        tokens.push(token);
                    }
                }
            }
        }
    }
    tokens.sort_unstable_by_key(|t| t.0);
    tokens.dedup_by_key(|t| t.0);
    tokens
}

impl<'a> MetadataResolver<'a> {
    /// Wraps an assembly to resolve the tokens of a single method (no inter-method calls).
    #[must_use]
    pub fn new(assembly: &'a Assembly<'a>) -> MetadataResolver<'a> {
        MetadataResolver {
            assembly,
            references: Vec::new(),
            rid_to_index: Vec::new(),
            type_arguments: Vec::new(),
            layout_arguments: Vec::new(),
            argument_assembly: None,
            method_arguments: Vec::new(),
            mono: crate::generics::MonoPlan::default(),
            box_target_tokens: box_target_tokens(assembly),
            type_init_thunks: Vec::new(),
            reference_owner: None,
        }
    }

    /// The assembly whose tokens this resolver reads -- the module being built, or, once
    /// [`rebased_on_reference`](Self::rebased_on_reference) has been applied, the REFERENCE whose
    /// CIL is being lowered into it.
    pub(crate) fn assembly(&self) -> &'a Assembly<'a> {
        self.assembly
    }

    /// This resolver READING the reference at `ordinal` instead of the module being built, for
    /// lowering that reference's own CIL into this module -- see [`ReferenceOwner`].
    ///
    /// **THE REFERENCE LIST IS TRUNCATED TO THE OWNER'S OWN**, `references[..ordinal]`: the lcsc
    /// layering the list already encodes says a reference resolves against the ones BEFORE it, which
    /// is exactly the list the owner's own library build was handed. That is what makes an identity
    /// the owner resolves through ITS references need no correction -- the same ordinal names the
    /// same assembly on both sides. Handing it the whole list instead would let the owner "resolve"
    /// a name through an assembly it was never built against, and resolve it to a DIFFERENT type
    /// than its own build did.
    ///
    /// The monomorphization plan is CARRIED, because it is keyed by canonical spelling and a
    /// spelling is assembly-independent; `rid_to_index` and the init-thunk indices are DROPPED,
    /// because both are this-module indices and the owner's rows do not occupy them.
    ///
    /// `None` when `ordinal` names no attached reference -- a plan built against a different
    /// reference list than the one being consumed, which is a wrong bind rather than a lookup miss.
    pub(crate) fn rebased_on_reference(
        &self,
        ordinal: u8,
        symbols: Vec<Option<alloc::string::String>>,
    ) -> Option<MetadataResolver<'a>> {
        let owner = *self.references.get(usize::from(ordinal))?;
        Some(MetadataResolver {
            assembly: owner,
            references: self.references[..usize::from(ordinal)].to_vec(),
            rid_to_index: Vec::new(),
            type_arguments: Vec::new(),
            layout_arguments: Vec::new(),
            argument_assembly: Some(self.assembly),
            method_arguments: Vec::new(),
            mono: self.mono.clone(),
            box_target_tokens: box_target_tokens(owner),
            type_init_thunks: Vec::new(),
            reference_owner: Some(ReferenceOwner::new(ordinal, symbols)),
        })
    }

    /// The initialization thunks a lowering may call at a trigger site, as `(TypeDef row, function
    /// index)` -- see [`precise_init_types`] for which types get one and why the relaxed ones do not.
    ///
    /// **ATTACH BEFORE LOWERING ANY BODY.** A resolver without this answers `None` everywhere, which
    /// is the eager tier's behavior: correct only while something else runs the initializers.
    #[must_use]
    pub fn with_type_init_thunks(mut self, thunks: Vec<(u32, u32)>) -> MetadataResolver<'a> {
        self.type_init_thunks = thunks;
        self
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

    /// The type arguments in force while lowering one monomorphized body.
    ///
    /// **THIS IS WHAT MAKES A MONOMORPHIZED BODY DIFFER FROM THE DEFINITION'S, AND IT CANNOT COME
    /// FROM THE TOKEN.** Inside `Box`1`'s own body a field access names its field by a plain `Field`
    /// token whose signature is `!0` -- there is no `TypeSpec` anywhere to decode, because the
    /// definition is talking about ITSELF. The instantiation is CONTEXT a caller supplies, not
    /// information the metadata carries at that point. (A field access from OUTSIDE, on
    /// `Box<int>`, does carry a TypeSpec parent, and that path is separate and already works.)
    ///
    /// Empty by default, which is exactly right for every non-generic body: a resolver with no
    /// arguments substitutes nothing, so the ordinary path is untouched.
    #[must_use]
    pub fn with_type_arguments(mut self, arguments: Vec<SigType>) -> MetadataResolver<'a> {
        self.layout_arguments.clone_from(&arguments);
        self.type_arguments = arguments;
        self
    }

    /// The LAYOUT reading of the arguments already in force, when it differs from the identity one.
    ///
    /// **THIS EXISTS BECAUSE A CROSS-ASSEMBLY BODY READS ITS ARGUMENTS IN THE WRONG WORLD
    /// OTHERWISE.** A monomorphized body declared next door is lowered under a resolver over its
    /// OWNER, and a type argument is a token of the CALLER's -- so a `ValueType` argument's width,
    /// field offsets and trace map get read from the owner's tables at the caller's row number,
    /// which is a real, unrelated type. [`caller_resolved_arguments`] re-expresses the arguments so
    /// no such read can happen, and this is where the result goes.
    ///
    /// **IT DOES NOT TOUCH [`Self::type_arguments`], AND THAT IS THE POINT.** The identity path
    /// (`closed_spec_signature` and everything that spells) must keep the argument the caller
    /// WROTE, or two instantiations collapse under one tag.
    #[must_use]
    pub fn with_layout_arguments(mut self, arguments: Vec<SigType>) -> MetadataResolver<'a> {
        self.layout_arguments = arguments;
        self
    }

    /// The assembly a TYPE ARGUMENT is written in, which is not always the one being read.
    ///
    /// For an ordinary resolver these are the same assembly. For one
    /// [`rebased_on_reference`](Self::rebased_on_reference) onto a definition's owner they are not:
    /// `self.assembly` becomes the OWNER and the arguments stay the CALLER's, which is the whole
    /// hazard this family exists to close.
    pub(crate) fn argument_world(&self) -> &'a Assembly<'a> {
        self.argument_assembly.unwrap_or(self.assembly)
    }

    /// The METHOD type arguments in force while lowering one generic method's monomorphized body --
    /// what `!!n` resolves to. The twin of [`with_type_arguments`](Self::with_type_arguments), and
    /// separate for the reason the field is: the two axes are indexed independently, so a body
    /// inside `Box<string>` calling `Pick<int>` needs both lists and one would collapse them.
    #[must_use]
    pub fn with_method_arguments(mut self, arguments: Vec<SigType>) -> MetadataResolver<'a> {
        self.method_arguments = arguments;
        self
    }

    /// Attaches the module's [`MonoPlan`](crate::generics::MonoPlan), so a call on a generic
    /// instantiation binds to the monomorphized body's function index.
    ///
    /// **THIS HAS TO BE ATTACHED BEFORE ANY BODY IS LOWERED, NOT AFTER.** The reachability walk
    /// finds a monomorphized body through the `Inst::Call` this map produces, so a resolver handed
    /// the plan late answers `None` to every instantiated call and the walk never learns those
    /// bodies exist. Collect, assign indices, THEN walk.
    #[must_use]
    pub fn with_monomorphized(mut self, plan: crate::generics::MonoPlan) -> MetadataResolver<'a> {
        self.mono = plan;
        self
    }

    /// How a call to a method THIS resolver's assembly declares is named -- the module's own
    /// function index, or, when the resolver is REBASED, an extern to the symbol the owner's own
    /// object defines.
    ///
    /// **THE REBASED ANSWER IS THE ONE THAT PREVENTS A LINK THAT MEANS SOMETHING ELSE.** `rid`
    /// is a row of the OWNER's `MethodDef` table, so an `Internal` target would name whatever sits
    /// at that index in the CALLER's function table -- an unrelated method, or a `stub()` that
    /// returns, and both link.
    ///
    /// **NAMED FROM THE OWNER'S OWN NAMING, NOT FROM A SECOND DERIVATION.** A library exports an
    /// accessible method under [`extern_method_symbol`] and keeps every other one as
    /// `L<hash>.f<rid>`, with a demotion rule for a mangled name two methods share -- three rules a
    /// rebase spelling its own would have to keep agreeing with. `symbols` is that function's own
    /// output, so there is nothing to keep agreeing with. A rid past its end -- or one the owner's
    /// object carries no BODY for -- is a REFUSAL, for which see [`ReferenceOwner::symbols`].
    fn own_call_target(&self, rid: u32) -> Option<CallTarget> {
        match &self.reference_owner {
            Some(owner) => Some(CallTarget::External(
                owner.symbols.get(rid as usize)?.as_deref()?.into(),
            )),
            None => Some(CallTarget::Internal(self.function_index(rid).unwrap_or(rid))),
        }
    }

    /// A call on a generic INSTANTIATION resolved to the monomorphized body's function index, or
    /// `None` when this is not such a call or the plan does not carry that body.
    ///
    /// **THE RETURN TYPE IS SUBSTITUTED HERE AND THAT IS NOT A DETAIL.** A `MemberRef` on a
    /// `TypeSpec` carries the DEFINITION's signature, so `Box<int>::Get`'s return type arrives as
    /// `!0`. Typing the call from that gives the caller a result with no MIR type at all -- and
    /// `Box<string>::Get` would type identically, which is the same "two instantiations answer the
    /// same" hole the layout side had to close. It is closed with the CALL SITE's arguments, which
    /// the `TypeSpec` carries.
    ///
    /// **A result whose type does not resolve REFUSES rather than lowering untyped.** An untyped
    /// value is not a smaller version of a typed one; it is a hole the verifier cannot see into.
    fn monomorphized_call(&self, token: Token, signature: &MethodSig) -> Option<CallInfo> {
        if self.mono.is_empty() || token.table() != table::MEMBER_REF {
            return None;
        }
        let member = self.assembly.member_ref(token.row())?;
        let spec = member.parent();
        if spec.table() != table::TYPE_SPEC {
            return None;
        }
        let open = self.assembly.type_spec_signature(spec)?;
        let name = crate::generics::spell_sig_across(
            self.assembly,
            self.argument_world(),
            &open,
            &self.type_arguments,
        )?;
        let index = self
            .mono
            .index_of(&name, member.name()?, &signature.parameters)?;
        let (_, _, arguments) = self.instantiated_parent(spec)?;
        self.monomorphized_call_info(index, signature, &arguments)
    }

    /// A call bound to a monomorphized body at `index`, typed for the caller: the shared tail of
    /// [`Self::monomorphized_call`] and [`Self::monomorphized_self_call`].
    ///
    /// **EXTRACTED RATHER THAN RESTATED, because the two differ only in HOW they find the
    /// instantiation and not in what they do with it.** One reads the arguments off a `TypeSpec`
    /// parent; the other is inside the definition and already holds them. Everything after that --
    /// the result substitution, the argument count, the target -- is one rule, and a second copy of
    /// it is how the result type comes to be substituted on one path and not the other.
    fn monomorphized_call_info(
        &self,
        index: u32,
        signature: &MethodSig,
        arguments: &[SigType],
    ) -> Option<CallInfo> {
        let has_result = !matches!(signature.return_type, SigType::Void);
        let result_type = if has_result {
            let arguments =
                caller_resolved_arguments(arguments, self.argument_world(), &self.references)?;
            let closed = crate::generics::substitute_sig(&signature.return_type, &arguments)?;
            Some(mir_type_across(
                &closed,
                self.assembly,
                self.argument_assembly,
                &self.references,
                &TargetLayout::ilp32(),
            )?)
        } else {
            None
        };
        Some(CallInfo {
            args: signature.parameters.len() + usize::from(signature.has_this),
            has_result,
            result_type,
            target: CallTarget::Internal(index),
        })
    }

    /// An IN-BODY SIBLING CALL spelled as a bare `MethodDef` -- `Add` calling `Grow()` from inside
    /// `List<T>` -- bound to the monomorphized body for the instantiation in force.
    ///
    /// **A DEFINITION CALLING ITS OWN MEMBER NEED NOT NAME AN INSTANTIATION, AND THE TWO SPELLINGS
    /// ARE BOTH LEGAL.** csc, and lcsc since `5fd7f35ea0`, emit a `MemberRef` parented by a
    /// `TypeSpec` over the type's own parameters, which [`Self::monomorphized_call`] resolves. An
    /// older lcsc emits the definition's own `MethodDef` row, which carries no instantiation at all
    /// -- the enclosing body supplies it. Without this arm that spelling reached `resolve_method`,
    /// bound to the OPEN definition's rid, and refused as `UnresolvedCall`.
    ///
    /// The key is built the way the `TypeSpec` path builds its own, through the SAME speller, so the
    /// two cannot disagree about the string the plan is holding: the declaring type instantiated over
    /// its own parameters (`` List`1<!0> ``), spelled with the arguments in force.
    ///
    /// **THE ARITY GUARD IS WHAT KEEPS THIS FROM ANSWERING FOR A TYPE IT IS NOT ABOUT.** A bare
    /// `MethodDef` naming a non-generic type's method, or a generic type of a different arity, spells
    /// a key the plan does not hold and misses -- but requiring the counts to match refuses it before
    /// the lookup rather than relying on a string not to collide.
    fn monomorphized_self_call(&self, token: Token, signature: &MethodSig) -> Option<CallInfo> {
        if self.mono.is_empty()
            || token.table() != table::METHOD_DEF
            || self.type_arguments.is_empty()
        {
            return None;
        }
        let declaring = self.type_token_of(token)?;
        let arity = self
            .assembly
            .generic_params()
            .filter(|&(_, _, owner, _)| owner & 1 == 0 && (owner >> 1) == declaring.row())
            .count();
        if arity != self.type_arguments.len() {
            return None;
        }
        let open = SigType::GenericInst {
            definition: alloc::boxed::Box::new(SigType::Class(declaring)),
            arguments: (0..self.type_arguments.len() as u32).map(SigType::Var).collect(),
        };
        let name = crate::generics::spell_sig_across(
            self.assembly,
            self.argument_world(),
            &open,
            &self.type_arguments,
        )?;
        let method = self.assembly.resolve_method(token)?;
        let index = self
            .mono
            .index_of(&name, method.name?, &signature.parameters)?;
        self.monomorphized_call_info(index, signature, &self.type_arguments)
    }

    /// A `callvirt` on a GENERIC INTERFACE instantiation -- `` IBox`1<int32>::Get() `` -- typed for
    /// the caller, with a PLACEHOLDER target the itable routing replaces.
    ///
    /// **THERE IS NO BODY TO BIND HERE AND THAT IS THE DIFFERENCE FROM [`Self::monomorphized_call`].**
    /// That one binds `` Box`1[System.Int32]::Get `` to the monomorphized body the plan emits. An
    /// INTERFACE method is abstract: the receiver's itable is what answers it, so what this has to
    /// produce is a correctly TYPED call whose target is overridden by `Inst::CallInterface`. An
    /// abstract method's own rid is the same placeholder a non-generic interface call already uses.
    ///
    /// **THE RESULT TYPE IS SUBSTITUTED, for `monomorphized_call`'s reason.** The `MemberRef`
    /// carries the definition's signature (II.22.25), so `` IBox`1<int32>::Get ``'s return type
    /// arrives as `!0`; typing the call from that gives the caller an untyped result, and
    /// `IBox<string>` would type identically.
    ///
    /// **AN INTERFACE DECLARED NEXT DOOR RESOLVES HERE TOO, AND ITS PLACEHOLDER IS A SYMBOL RATHER
    /// THAN A RID** -- which is `IEnumerable<T>` arriving from a BCL rather than from the program.
    /// [`Self::instantiated_parent`] resolves the spec to the OWNER's `TypeDef`, so the interface
    /// has a name to be spelled by even though a `TypeSpec` carries none itself.
    ///
    /// **THE PLACEHOLDER MUST NOT BE [`Self::own_call_target`] THERE, AND THAT IS THE WHOLE
    /// JUDGEMENT.** `rid` indexes the OWNER's `MethodDef` table, so a this-assembly function index
    /// names whatever sits at that row HERE -- an unrelated method, and one that LINKS. The
    /// reference arm names the interface method's own extern symbol instead, which NOTHING EXPORTS,
    /// an interface method having no body in any object. The ordinary path discards it, since the
    /// itable routing replaces the instruction; the one path that would use it -- a tag that failed
    /// to compute, falling through to a direct call -- is a link error rather than a wrong answer.
    fn instantiated_interface_call(
        &self,
        token: Token,
        signature: &MethodSig,
    ) -> Option<CallInfo> {
        if token.table() != table::MEMBER_REF {
            return None;
        }
        let member = self.assembly.member_ref(token.row())?;
        let spec = member.parent();
        if spec.table() != table::TYPE_SPEC {
            return None;
        }
        let (owner, type_def, arguments) = self.instantiated_parent(spec)?;
        if !type_def.is_interface() {
            return None;
        }
        let own = core::ptr::eq(owner, self.assembly);
        let name = member.name()?;
        let key = param_key(
            self.assembly,
            signature.generic_param_count,
            &signature.parameters,
        );
        let declared = type_def.methods().find(|method| {
            method.name() == Some(name)
                && method.signature().is_some_and(|sig| {
                    param_key(owner, sig.generic_param_count, &sig.parameters) == key
                })
        })?;
        let rid = declared.rid();
        let has_result = !matches!(signature.return_type, SigType::Void);
        let result_type = if has_result {
            let arguments =
                caller_resolved_arguments(&arguments, self.argument_world(), &self.references)?;
            let closed = crate::generics::substitute_sig(&signature.return_type, &arguments)?;
            Some(mir_type_across(
                &closed,
                self.assembly,
                self.argument_assembly,
                &self.references,
                &TargetLayout::ilp32(),
            )?)
        } else {
            None
        };
        let target = if own {
            self.own_call_target(rid)?
        } else {
            let iface_name = owner.type_token_name(type_def.token())?;
            let iface_signature = decodable_signature(&declared)?;
            CallTarget::External(
                extern_method_symbol(
                    iface_name.namespace,
                    iface_name.name,
                    name,
                    &iface_signature.parameters,
                    &iface_signature.return_type,
                    &|token| owner.type_token_name(token).map(|n| joined_full_name(&n)),
                )
                .into(),
            )
        };
        Some(CallInfo {
            args: signature.parameters.len() + usize::from(signature.has_this),
            has_result,
            result_type,
            target,
        })
    }

    /// The GENERIC METHOD twin of [`Self::monomorphized_call`]: a call site naming a `MethodSpec`
    /// binds to the body planned for that exact `(method, arguments)` pair.
    ///
    /// **THE TOKEN IS THE KEY AND NOTHING IS SPELLED.** A `MethodSpec` row IS the pair, so unlike
    /// the type axis there is no canonical name to compare and no overload to tell apart -- the
    /// call site and the plan hold the same token.
    ///
    /// **A `MethodSpec` THE PLAN DOES NOT CARRY ANSWERS `None`, AND THAT IS THE WHOLE SAFETY
    /// PROPERTY.** `resolve` then finds no method for the token at all (`resolve_method` handles
    /// `MethodDef` and `MemberRef` only) and refuses `UnresolvedCall`. There is deliberately no arm
    /// that resolves a `MethodSpec` to the `MethodDef` it names: that fallback binds a VIRTUAL
    /// generic call to the base's declaration, producing a program that links, runs, and calls the
    /// wrong override on a derived receiver -- with nothing anywhere reporting it. The refusal to
    /// plan a virtual one (in `MonoPlan::method_axis`) and the absence of a fallback here are two
    /// halves of one guarantee, and either alone would leave the hole open.
    ///
    /// **The result type is substituted with the CALL SITE's method arguments**, for the reason the
    /// type axis substitutes with the TypeSpec's: a generic definition's signature returns `!!0`,
    /// and typing the call from that gives every instantiation the same answer.
    fn monomorphized_method_call(&self, token: Token) -> Option<CallInfo> {
        if self.mono.is_empty() || token.table() != table::METHOD_SPEC {
            return None;
        }
        if self.reference_owner.is_some() {
            return None;
        }
        let index = match self.mono.method_index_of(token) {
            Some(index) => index,
            None => match self.virtual_generic_dispatch(token)? {
                GenericDispatch::Slot(_) => self.mono.virtual_method_index_of(token)?,
                GenericDispatch::Tag(_) => self.assembly.method_spec_method(token)?.row(),
            },
        };
        let definition = self.assembly.method_spec_method(token)?;
        let arguments = self.assembly.method_spec_instantiation(token)?;
        let signature = self.assembly.resolve_method(definition)?.signature?;
        let has_result = !matches!(signature.return_type, SigType::Void);
        let result_type = if has_result {
            let closed =
                crate::generics::substitute_sig_with(&signature.return_type, &[], &arguments)?;
            Some(mir_type(&closed, self.assembly, &TargetLayout::ilp32())?)
        } else {
            None
        };
        Some(CallInfo {
            args: signature.parameters.len() + usize::from(signature.has_this),
            has_result,
            result_type,
            target: CallTarget::Internal(index),
        })
    }

    /// How a `callvirt` at a `MethodSpec` naming a VIRTUAL generic method reaches the body that must
    /// actually run -- or `None`, which is a refusal and never a fallback.
    ///
    /// # This is asked in three places and answered in one
    ///
    /// [`Self::monomorphized_method_call`] asks whether a dispatch exists at all before it will type
    /// the call; [`Self::virtual_slot`] and [`Self::interface_call_tag`] each take the half they
    /// emit. Three separate derivations of the same judgement is how a call site comes to type
    /// itself against a dispatch that was never built, which on this shape is a DIRECT call to the
    /// base's body: a program that links, runs and answers 5 where 42 is correct.
    ///
    /// # What it refuses, and why each refusal is a wrong answer rather than a missing feature
    ///
    /// * **A definition declared NEXT DOOR** (the `MethodSpec` names a `MemberRef`). The owning
    ///   assembly numbered its own vtable from its own metadata, with no view of this module's call
    ///   sites, so it laid no slot for this instantiation -- a `callvirt` computed against a
    ///   numbering this build invented would index past the end of a real object's table.
    /// * **An EXPLICIT interface implementation.** `Program::close_over_overrides` states outright
    ///   that it does not cover one: `int ITag.Tag<T>()` is named through `MethodImpl` under a
    ///   mangled name and is a third LOOKUP. So the plan holds no body for it, and this must not
    ///   read the closure's answer as though it did.
    /// * **A pair the plan does not carry**, which is every shape `MonoPlan::method_axis` declined.
    fn virtual_generic_dispatch(&self, token: Token) -> Option<GenericDispatch> {
        if token.table() != table::METHOD_SPEC {
            return None;
        }
        if self.reference_owner.is_some() {
            return None;
        }
        let definition = self.assembly.method_spec_method(token)?;
        if definition.table() != table::METHOD_DEF {
            return None;
        }
        let method = self.assembly.method(definition.row())?;
        let name = method.name()?;
        let signature = decodable_signature(&method)?;
        let arguments = self.assembly.method_spec_instantiation(token)?;
        let key = instantiated_slot_key(
            self.assembly,
            &param_key(
                self.assembly,
                signature.generic_param_count,
                &signature.parameters,
            ),
            &arguments,
        );
        let type_token = self.type_token_of(definition)?;
        if type_token.table() != table::TYPE_DEF {
            return None;
        }
        let type_def = self.assembly.type_def(type_token.row())?;
        if type_def.is_interface() {
            if self.has_explicit_implementation(definition) {
                return None;
            }
            let iface_name = self.assembly.type_token_name(type_token)?;
            return instantiated_interface_method_tag(
                self.assembly,
                &iface_name,
                name,
                &signature.parameters,
                self.assembly,
                &arguments,
            )
            .map(GenericDispatch::Tag);
        }
        if method.body().is_some() {
            let declaring = crate::generics::type_def_full_name(self.assembly, type_token)?;
            self.mono
                .virtual_method_body(&declaring, name, &arguments)?;
        }
        self.vtable_methods(type_def)
            .iter()
            .position(|slot| slot.name == Some(name) && slot.key == key)
            .map(GenericDispatch::Slot)
    }

    /// Whether any type in this module implements the interface method at `declaration` EXPLICITLY
    /// -- a `MethodImpl` row naming it.
    ///
    /// **THE COMPARISON IS BY RESOLVED IDENTITY, NOT BY TOKEN**, because a `MethodImpl`'s
    /// declaration column is a `MethodDefOrRef` (II.22.27): the same interface method is a
    /// `MethodDef` when the interface is this module's and a `MemberRef` when it is not, and
    /// comparing raw tokens would answer "no explicit implementation" for the second -- the exact
    /// false negative this exists to prevent.
    fn has_explicit_implementation(&self, declaration: Token) -> bool {
        let Some(target) = self.assembly.resolve_method(declaration) else {
            return true;
        };
        self.assembly.type_defs().any(|type_def| {
            type_def.method_impls().any(|(_, declared)| {
                self.assembly
                    .resolve_method(declared)
                    .is_some_and(|other| {
                        other.name == target.name
                            && other.declaring_type == target.declaring_type
                    })
            })
        })
    }

    /// `ty` with this resolver's instantiation applied, or `ty` unchanged when none is in force.
    ///
    /// **A type parameter with NO argument in force answers `None` rather than passing through.**
    /// `!0` reaching a layout with nothing to substitute is not a type that happens to be generic --
    /// it is a body being lowered without the context it needed, and letting it through would size a
    /// field by whatever the layout code makes of an unresolved parameter.
    ///
    /// Both axes at once: `!n` from the enclosing type's arguments, `!!n` from the method's. A body
    /// lowered under one axis passes an empty list for the other, so an unresolvable parameter
    /// refuses exactly as it did when only the type axis existed.
    fn apply_instantiation(&self, ty: &SigType) -> Option<SigType> {
        if self.layout_arguments.is_empty() && self.method_arguments.is_empty() {
            return Some(ty.clone());
        }
        crate::generics::substitute_sig_with(ty, &self.layout_arguments, &self.method_arguments)
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
        Assembly::find_in_references(&self.references, namespace, name)
            .map(|(ordinal, type_def)| (ordinal, self.references[ordinal], type_def))
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
            if let Some((namespace, name)) = self.assembly.type_token_full_name(token) {
                if let Some((ordinal, _, ref_td)) = self.find_reference_type(&namespace, &name) {
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
            type_arguments: Vec::new(),
            layout_arguments: Vec::new(),
            argument_assembly: None,
            method_arguments: Vec::new(),
            mono: crate::generics::MonoPlan::default(),
            box_target_tokens: box_target_tokens(assembly),
            type_init_thunks: Vec::new(),
            reference_owner: None,
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
            let Some((namespace, name)) = self.assembly.type_token_full_name(current) else {
                return false;
            };
            if namespace == "System" && (name == "Exception" || name.ends_with("Exception")) {
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

    /// The exception tag of a CONSTRUCTED type -- what `throw new MyError<int>()` puts in flight and
    /// what `catch (MyError<int>)` matches against.
    ///
    /// A `TypeSpec` has no name of its own, so `type_token_name` answers `None` for one and every
    /// tag path keyed on a name declines. A constructed type's identity is its canonical SPELLING,
    /// and its tag is [`exception_tag_for_name`] of that spelling in the NAME position with the
    /// namespace empty -- the same three steps, and therefore the same tag, that
    /// [`Self::instantiation_descriptors`] already writes into the descriptor. The throw side and
    /// the catch side meet by construction rather than by agreement.
    ///
    /// Whether a constructed type is an exception at all is decided by its DEFINITION:
    /// `MyError<int>` is one exactly when `MyError<T>` extends `System.Exception`. A definition
    /// owned by another assembly is followed no further than its name, which is
    /// [`Self::is_exception_type`]'s own rule for a `TypeRef` rather than a second one written
    /// here.
    fn instantiation_exception_tag(&self, type_token: Token) -> Option<u32> {
        if type_token.table() != table::TYPE_SPEC {
            return None;
        }
        let SigType::GenericInst { definition, .. } =
            self.assembly.type_spec_signature(type_token)?
        else {
            return None;
        };
        let (SigType::Class(definition) | SigType::ValueType(definition)) = definition.as_ref()
        else {
            return None;
        };
        if !self.is_exception_type(*definition) {
            return None;
        }
        let closed = self.closed_spec_signature(type_token)?;
        let spelled = crate::generics::spell_sig(self.assembly, &closed)?;
        let tag = exception_tag_for_name("", &spelled);
        (tag != 0).then_some(tag)
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
                if let Some((base_ns, base_name)) = self.assembly.type_token_full_name(base) {
                    if let Some((_, reference, ref_td)) =
                        self.find_reference_type(&base_ns, &base_name)
                    {
                        slots = reference_vtable_slots(&self.references, reference, ref_td);
                    }
                }
            }
        }
        for td in chain.into_iter().rev() {
            let declaring = self.declaring_full_name(td);
            for method in td.methods() {
                if !method.is_virtual() {
                    continue;
                }
                let name = method.name();
                let rid = method.rid();
                let key = slot_key(self.assembly, &method, rid);
                let newslot = method.flags() & 0x0100 != 0;
                for (key, impl_) in self.slots_for(declaring.as_deref(), &method, rid, key) {
                    if !newslot {
                        if let Some(entry) = slots
                            .iter_mut()
                            .find(|slot| slot.name == name && slot.key == key)
                        {
                            entry.impl_ = impl_;
                            continue;
                        }
                    }
                    slots.push(VSlot { name, key, impl_ });
                }
            }
        }
        slots
    }

    /// The full name of a this-assembly type, spelled by the ONE function that spells the name a
    /// [`MonoMethodBody`](crate::generics::MonoMethodBody) carries.
    ///
    /// A second spelling of a nested chain here would make a dispatch slot look up a body under a
    /// name the plan never wrote, which is a slot silently left unimplemented rather than an error.
    fn declaring_full_name(&self, type_def: TypeDef<'a>) -> Option<String> {
        crate::generics::type_def_full_name(self.assembly, type_def.token())
    }

    /// The vtable slots ONE declared virtual method occupies: ordinarily exactly one, and for a
    /// VIRTUAL GENERIC METHOD one per argument list the module calls it at.
    ///
    /// # Why a generic virtual needs more than one slot, and why that is the cheap arm here
    ///
    /// A slot holds ONE function address, and `Tag<int>` and `Tag<string>` are two bodies -- so one
    /// slot cannot serve both. The alternative is a per-call lookup keyed by signature, which suits
    /// a tier whose tables are frozen once built; these are rebuilt whole on every compile, so the
    /// cost of a slot is paid at build time and a lookup would be paid on every call.
    ///
    /// # The key gains a component and no encoding freezes
    ///
    /// [`param_key`]'s own note records the measurement: the slot key is a COMPARISON key, never
    /// serialized -- the link name and the interface tag are computed elsewhere -- so it may gain a
    /// component without touching anything a later build must reproduce. This is that component.
    ///
    /// # What it deliberately does NOT expand, and the refusal that stands behind it
    ///
    /// A declaration the module calls at NO argument list keeps its single open slot, unchanged
    /// (`genarity` declares a generic virtual nobody calls, and its numbering must not move). And a
    /// slot INHERITED from a reference is never expanded here at all: it was numbered by the owning
    /// assembly, from ITS metadata, with no view of this module's call sites -- so growing it would
    /// leave the library's own emitted table shorter than the numbering a `callvirt` computes
    /// against, which indexes past the end of a real object's vtable. The call site refuses that
    /// shape instead, because it finds no argument-keyed slot to dispatch through.
    fn slots_for(
        &self,
        declaring: Option<&str>,
        method: &lamella_metadata::Method<'a>,
        rid: u32,
        key: String,
    ) -> Vec<(String, SlotImpl)> {
        let Some(signature) = decodable_signature(method) else {
            return alloc::vec![(key, SlotImpl::Rid(rid))];
        };
        if signature.generic_param_count == 0 {
            return alloc::vec![(key, SlotImpl::Rid(rid))];
        }
        let Some(name) = method.name() else {
            return alloc::vec![(key, SlotImpl::Rid(rid))];
        };
        let instantiations = self.mono.virtual_method_instantiations(
            name,
            signature.generic_param_count,
            &signature.parameters,
        );
        if instantiations.is_empty() {
            return alloc::vec![(key, SlotImpl::Rid(rid))];
        }
        instantiations
            .into_iter()
            .map(|arguments| {
                let keyed = instantiated_slot_key(self.assembly, &key, arguments);
                let impl_ = match declaring
                    .and_then(|declaring| self.mono.virtual_method_body(declaring, name, arguments))
                {
                    Some(index) => SlotImpl::Mono(index),
                    None => SlotImpl::Rid(rid),
                };
                (keyed, impl_)
            })
            .collect()
    }

    /// Every this-module `TypeDef` an image may lay a DESCRIPTOR for, which is every type except an
    /// OPEN GENERIC DEFINITION.
    ///
    /// **AN OPEN DEFINITION IS NOT A RUNTIME TYPE.** `` Box`1 `` cannot be allocated, cast to or
    /// caught; only its INSTANTIATIONS can, and each of those lays its own through
    /// [`Self::instantiation_descriptors`]. A descriptor laid for the definition puts its own
    /// methods' rids in a vtable, and those are the one set of bodies this tier cannot lower -- an
    /// open body's `!0` sizes nothing -- so the slot points at a body that does not exist.
    ///
    /// **AND IT IS WHAT KEEPS THE REACHABILITY WALK'S STATED INVARIANT TRUE.** That walk never
    /// reaches an open definition because every CALL SITE binds to a monomorphized index instead --
    /// a premise written down where the lowering skips those bodies. A vtable entry on the
    /// definition's own descriptor is a reference that is not a call site, so without this filter
    /// the walk reaches an open body and the build refuses `BadOperand` on it. That is not
    /// hypothetical: it is what a generic type implementing a GENERIC INTERFACE produces, because
    /// an interface implementation is virtual where an ordinary method is not.
    ///
    /// Read from the `GenericParam` rows rather than from a backtick in the name: an arity suffix
    /// is a naming convention this tier does not own, and a type could carry one without being
    /// generic.
    fn descriptor_type_defs(&self) -> Vec<TypeDef<'a>> {
        let generic_definitions = self.assembly.type_parameter_names();
        self.assembly
            .type_defs()
            .filter(|type_def| !generic_definitions.contains_key(&type_def.token().row()))
            .collect()
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
        for type_def in self.descriptor_type_defs() {
            let methods = self.vtable_methods(type_def);
            if methods.is_empty() {
                continue;
            }
            if let Some(entries) = slot_entries(&methods, &|rid| self.function_index(rid)) {
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
    /// The key is NAME plus the identity of a NON-GENERIC, PARAMETERLESS signature, which is what
    /// distinguishes `ToString()` from an overload of it -- and from a `ToString<T>()`, which is a
    /// legal overload of exactly this shape. `None` when the type has no such slot (it inherits no
    /// virtuals -- e.g. an enum built with no corlib attached, whose base `System.Enum` cannot be
    /// resolved).
    ///
    /// **THE KEY IS ASKED OF [`param_key`] RATHER THAN SPELLED HERE.** A predicate that spells the
    /// key's FORMAT in a second place (`slot.key.is_empty()`) stops matching anything at all the
    /// moment the format gains a component, and it does so SILENTLY: an enum's synthesized
    /// `ToString` misses Object's slot, its body dead-strips, and the image comes out SMALLER with
    /// no gate red, because the size gate fails on growth and that is a shrink. Making the arity a
    /// required parameter enumerates the CALLERS of `param_key` and cannot reach a site that does
    /// not call it. **A format has one implementation or it has none.**
    #[must_use]
    pub fn nullary_vtable_slot(&self, type_def: TypeDef<'a>, name: &str) -> Option<usize> {
        let key = param_key(self.assembly, 0, &[]);
        self.vtable_methods(type_def)
            .iter()
            .position(|slot| slot.name == Some(name) && slot.key == key)
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
            .descriptor_type_defs()
            .into_iter()
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

    /// EVERY descriptor an image lays: this assembly's own types, then one per INSTANTIATION the
    /// attached [`MonoPlan`](crate::generics::MonoPlan) covers.
    ///
    /// **THE UNION IS ONE FUNCTION SO THAT A BUILD PATH CANNOT HOLD HALF OF IT.** A path that lays
    /// this assembly's descriptors and not the plan's gives every instantiation TAG ZERO -- one
    /// identity shared by all of them, which is exactly what `isinst`/`castclass` compares -- and,
    /// once instantiations carry dispatch tables, an EMPTY VTABLE. **Neither is visible in an object
    /// file that links and boots.** Assembled here, a path can only be missing it by not calling
    /// this at all.
    ///
    /// A resolver with no plan attached answers exactly [`Self::type_descriptors`], which is what
    /// keeps every non-generic build on the bytes it was already on.
    #[must_use]
    pub fn image_descriptors(&self) -> Vec<TypeMeta> {
        let mut descriptors = self.type_descriptors();
        descriptors.extend(self.instantiation_descriptors());
        descriptors
    }

    /// A [`TypeMeta`] per INSTANTIATION the attached [`MonoPlan`](crate::generics::MonoPlan) covers.
    ///
    /// # WHAT WAS ACTUALLY MISSING WAS THE TAG, NOT THE DESCRIPTOR
    ///
    /// An `Inst::Alloc` already carries the SUBSTITUTED payload size and reference offsets, and the
    /// emitters build a descriptor's words straight out of them -- so an instantiation's descriptor
    /// was being laid, with the right size and the right GC trace map. What it took from the
    /// resolver was the `type_tag`, by handle, and an instantiation had no entry: **`map_or(0, ..)`,
    /// so every instantiation's descriptor carried TAG ZERO.**
    ///
    /// **A zero tag is not a missing feature, it is the collapsed identity arriving through the
    /// tag word.** The tag is what `isinst`/`castclass` compares and what a catch clause matches, so
    /// `Box<int>` and `Box<string>` -- and every other type the table does not name -- would all
    /// answer as one identity. The Alloc site's own comment reasons that a type with no entry is
    /// harmless because "only virtual dispatch on it would be undefined"; that holds for the VTABLE
    /// and does not hold for the TAG.
    ///
    /// The tag comes from [`exception_tag_for_name`] of the canonical spelling -- the SAME function
    /// and therefore the same tag space every non-generic type's identity already comes from, which
    /// is why an instantiation needs no new hash and no new tag space.
    ///
    /// **`vtable` and `itable` COME FROM [`Self::instantiation_dispatch`], AND AN INSTANTIATION
    /// THAT CANNOT PRODUCE THEM IS DROPPED HERE RATHER THAN DESCRIBED WITHOUT THEM.** An empty
    /// vtable does not fail -- it dispatches to nothing, silently -- so "cannot describe" and
    /// "describe with no slots" must not be the same answer. Dropping is safe only because
    /// `build::refuse_undispatchable_instantiations` compares this table against the plan and
    /// REFUSES THE BUILD for anything missing: on its own, a drop here is a filter, and a filter
    /// let a program that hard-faults out of the door once already.
    ///
    /// **`exported` is `false` deliberately.** It only controls whether a LIBRARY lays a
    /// descriptor PROACTIVELY for a type it never allocates. An instantiation's symbol is
    /// assembly-independent by definition, so two libraries laying one proactively would define one
    /// symbol twice; a build lays it because it REACHES it, which is one definition per image.
    #[must_use]
    pub fn instantiation_descriptors(&self) -> Vec<TypeMeta> {
        self.mono
            .instantiations()
            .into_iter()
            .filter_map(|(name, spec)| {
                let layout = self.instantiated_reference_layout(spec)?;
                let (vtable, itable) = self.instantiation_dispatch(name, spec)?;
                let mut words = alloc::vec![
                    layout.size,
                    layout.reference_offsets.len() as u32,
                    exception_tag_for_name("", name),
                    0,
                ];
                words.extend(layout.reference_offsets.iter().copied());
                Some(TypeMeta {
                    handle: layout.handle,
                    type_tag: exception_tag_for_name("", name),
                    vtable,
                    itable,
                    base: None,
                    words: Some(words.into_boxed_slice()),
                    exported: false,
                    full_name: Some(alloc::boxed::Box::from(name)),
                })
            })
            .collect()
    }

    /// Every instantiation the attached plan carries that NEEDS a descriptor and did not get one
    /// from [`Self::instantiation_descriptors`] -- the set `build::refuse_undispatchable_instantiations`
    /// turns into a build error.
    ///
    /// **IT DIFFS AGAINST THE TABLE ACTUALLY PRODUCED RATHER THAN RE-DERIVING WHY.** Whatever
    /// [`Self::instantiation_dispatch`] declines tomorrow is refused tomorrow, with no second
    /// predicate to keep in step -- which is the whole reason the refusal moved out of the layout
    /// path in the first place.
    ///
    /// **A VALUE-TYPE INSTANTIATION IS NOT MISSING A DESCRIPTOR, IT HAS NONE TO MISS.** `Holder<int>`
    /// is laid IN a slot, sized and traced by [`instantiated_value_type_slot`]; nothing allocates it
    /// on the heap and nothing dispatches through it, so there is no descriptor and refusing for the
    /// absence of one would refuse every generic struct that declares a method. The applicability
    /// rule is stated here, once, rather than being an accident of which fixtures have methods.
    #[must_use]
    pub fn undescribed_instantiations(&self) -> Vec<alloc::boxed::Box<str>> {
        let described = self.instantiation_descriptors();
        self.mono
            .instantiations()
            .into_iter()
            .filter(|(_, spec)| {
                self.instantiated_parent(*spec)
                    .is_some_and(|(_, type_def, _)| !type_def.is_value_type())
            })
            .filter(|(name, _)| {
                !described
                    .iter()
                    .any(|meta| meta.full_name.as_deref() == Some(*name))
            })
            .map(|(name, _)| alloc::boxed::Box::from(name))
            .collect()
    }

    /// ONE INSTANTIATION'S DISPATCH TABLES: its vtable in slot order and its itable keyed by
    /// interface-method tag, both naming the MONOMORPHIZED bodies the plan already emits.
    ///
    /// # The vtable is numbered on the OPEN DEFINITION, and the call site leaves no choice
    ///
    /// A slot's identity is `(name, parameter key)` and the key is computed from the method's
    /// PARAMETERS, which substitution changes -- so numbering each instantiation from its own
    /// substituted signatures would give `Box<int>` and `Box<string>` different layouts. The caller
    /// cannot follow that: a `callvirt` on an instantiation names a `MemberRef` parented by the
    /// `TypeSpec`, whose signature is the DEFINITION's verbatim (ECMA-335 II.22.25), and a call
    /// through a BASE-typed reference names the base's own method. Both derive the slot from the
    /// definition's numbering, which is what [`Self::vtable_methods`] answers, so this asks the same
    /// function every ordinary type's table is built from. Numbering it any other way would leave
    /// the two sides agreeing only by accident.
    ///
    /// # What differs from an ordinary type is ONE mapping, and it is the parameter
    ///
    /// A slot the DEFINITION ITSELF declares resolves to the plan's body for
    /// `(instantiation, method, declared parameters)` -- **never to the definition's own rid**,
    /// which is a `stub()` that returns, because an open definition's body is never emitted. A slot
    /// inherited from a base in this assembly keeps its ordinary function index; one inherited from
    /// a REFERENCE keeps its extern symbol, exactly as [`Self::vtables`] leaves it.
    ///
    /// # A definition declared NEXT DOOR is the same rule read against the OWNER's tables
    ///
    /// **This is the shape a BCL generic takes**, so it is what stands between this tier and
    /// `List<T>`. Its rows -- bases, interfaces, method names, `MethodImpl` pairs -- live in the
    /// OWNER's tables and index nothing here, so the numbering comes from
    /// [`reference_vtable_slots`] over the owner and [`Self::interface_entries`] is pointed at the
    /// owner too. Only one thing is genuinely different from a reference-owned ORDINARY type: that
    /// walk names every slot by the symbol the owning object exports, and for a slot the OPEN
    /// DEFINITION declares no such symbol exists, because the library lowering skips an open
    /// definition's bodies. Those slots come back onto the rid path and reach the plan's
    /// monomorphized body; the inherited ones keep their externs.
    ///
    /// # What it refuses, and why each refusal is a wrong answer rather than a missing feature
    ///
    /// * **A base spelled as an instantiation** (`class C<T> : Base<T>`), in EITHER assembly. The
    ///   layout walk stops at a `TypeSpec` base, so the descriptor would omit the base's fields AND
    ///   its slots.
    /// * **A slot with no implementation to name** -- an abstract method of the definition (the plan
    ///   emits no body for a method with no CIL), or an inherited rid that is not a function of this
    ///   module. On the owner arm this also covers a rid that is not the definition's own at all,
    ///   where the this-assembly fallback would name an unrelated method that LINKS.
    ///
    /// `None` for every one of those, and the caller turns it into a build error rather than an
    /// image.
    fn instantiation_dispatch(
        &self,
        name: &str,
        spec: Token,
    ) -> Option<(Vec<VtableEntry>, Vec<(u32, VtableEntry)>)> {
        let (owner, type_def, arguments) = self.instantiated_parent(spec)?;
        let own = core::ptr::eq(owner, self.assembly);
        let mut slots = if own {
            self.vtable_methods(type_def)
        } else {
            reference_vtable_slots(&self.references, owner, type_def)
        };
        if !own {
            for slot in &mut slots {
                let declared = type_def.methods().find(|method| {
                    method.is_virtual()
                        && method.name() == slot.name
                        && slot_key(owner, method, method.rid()) == slot.key
                });
                if let Some(method) = declared {
                    slot.impl_ = SlotImpl::Rid(method.rid());
                }
            }
        }
        let resolve = |rid: u32| -> Option<u32> {
            match type_def.methods().find(|method| method.rid() == rid) {
                Some(method) => {
                    self.mono
                        .index_of(name, method.name()?, &decodable_params(&method)?)
                }
                None => own.then(|| self.function_index(rid)).flatten(),
            }
        };
        let vtable = slot_entries(&slots, &resolve)?;
        let itable = self.interface_entries(owner, type_def, &slots, &arguments, &resolve);
        Some((vtable, itable))
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
        let slots = reference_vtable_slots(&self.references, reference, type_def);
        let vtable = slot_entries(&slots, &|rid| Some(rid))?;
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
        for ChainLink {
            assembly: link_assembly,
            type_def: link,
            ..
        } in self.cross_class_chain(reference, type_def, &[])
        {
            for iface_token in link.interfaces() {
                let Some((iface_assembly, iface, identity)) =
                    self.interface_link(link_assembly, iface_token, &[])
                else {
                    continue;
                };
                let iface_name = identity.type_name();
                for method in iface.methods() {
                    let Some(name) = method.name() else { continue };
                    let Some(signature) = decodable_signature(&method) else {
                        continue;
                    };
                    let params = signature.parameters;
                    let Some(tag) = interface_method_tag(iface_assembly, &iface_name, name, &params)
                    else {
                        continue;
                    };
                    let key = param_key(iface_assembly, signature.generic_param_count, &params);
                    let Some(slot) = impls
                        .iter()
                        .find(|slot| slot.name == Some(name) && slot.key == key)
                    else {
                        continue;
                    };
                    if entries.iter().any(|(t, _)| *t == tag) {
                        continue;
                    }
                    let Some(entry) = slot_entry(slot, &|rid| Some(rid)) else {
                        continue;
                    };
                    entries.push((tag, entry));
                }
            }
        }
        let links: Vec<(&'a Assembly<'a>, TypeDef<'a>)> = self
            .cross_class_chain(reference, type_def, &[])
            .into_iter()
            .map(|link| (link.assembly, link.type_def))
            .collect();
        self.fold_explicit_itable_entries(
            &links,
            &|link, link_type, body| {
                let MethodKind::Definition(rid) = link.resolve_method(body)?.kind else {
                    return None;
                };
                let method = link_type.methods().find(|method| method.rid() == rid)?;
                let key = slot_key(link, &method, rid);
                let slot = impls
                    .iter()
                    .find(|slot| slot.name == method.name() && slot.key == key)?;
                slot_entry(slot, &|rid| Some(rid))
            },
            &mut entries,
        );
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
    ///
    /// `assembly` is the one `type_def`'s ROWS ARE WRITTEN IN, which is this module's for an
    /// ordinary type and the OWNER's for an instantiation of a definition declared next door. Every
    /// token read here -- a base row, an interface row -- indexes that assembly's tables and means
    /// nothing in any other, so it is a parameter rather than `self.assembly`.
    fn interface_closure(&self, assembly: &'a Assembly<'a>, type_def: TypeDef<'a>) -> Vec<Token> {
        let mut queue: Vec<Token> = type_def.interfaces().collect();
        let mut base = type_def.extends();
        for _ in 0..64 {
            if base.row() == 0 || base.table() != table::TYPE_DEF {
                break;
            }
            let Some(base_def) = assembly.type_def(base.row()) else {
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
                if let Some(iface) = assembly.type_def(token.row()) {
                    queue.extend(iface.interfaces());
                }
            }
        }
        closed
    }

    /// The INTERFACE a `MethodImpl` row's declaration column names and the dispatch TAG it carries,
    /// or `None` if the row is not interface dispatch at all.
    ///
    /// The two travel together because a caller walking a base chain needs both and they come from one
    /// resolution: the tag keys the itable entry, and the interface name is what decides whether a more
    /// derived type has RE-IMPLEMENTED the mapping this row establishes.
    ///
    /// `MethodImpl` covers BOTH explicit interface implementations and explicit overrides of a base
    /// CLASS's virtual, and only the first belongs in an itable -- a class virtual is a vtable SLOT, and
    /// putting it in the itable would key a slot by a tag no `callvirt` derives. So the declaring type
    /// must actually be an interface, checked in this assembly first and then across the references, and
    /// anything undecidable is DECLINED rather than guessed.
    ///
    /// `assembly` is the one the `MethodImpl` row and its declaration token are written in -- this
    /// module's for an ordinary type, the OWNER's for a type declared next door.
    fn explicit_interface_dispatch(
        &self,
        assembly: &'a Assembly<'a>,
        declaration: Token,
    ) -> Option<(String, u32)> {
        let declared = assembly.resolve_method(declaration)?;
        let interface = declared.declaring_type?;
        let is_interface = match assembly.find_type(interface.namespace, interface.name) {
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
        let tag = interface_method_tag(assembly, &interface, declared.name?, &signature.parameters)?;
        Some((joined_full_name(&interface), tag))
    }

    /// Folds every EXPLICIT (`MethodImpl`) interface implementation reachable from a type into
    /// `entries`, overriding any entry already keyed by the same tag.
    ///
    /// **`links` IS THE BASE CHAIN, DERIVED-FIRST, AND THAT IS THE WHOLE POINT OF THIS FUNCTION.** An
    /// interface map is established at the type whose `InterfaceImpl` row declares it and is INHERITED
    /// by everything below, so `class D : B` answers for `IFoo` exactly as `B : IFoo` does -- and the
    /// `MethodImpl` rows that say HOW are written on `B`, never copied down. Reading only `D`'s own rows
    /// therefore leaves `D`'s itable missing every explicitly implemented interface its base declares,
    /// and an itable that is missing an entry is not a dispatch that fails to link: the emitted scan
    /// runs off the end of the table and traps (`unreachable` on wasm, `udf` on Cortex-M), and
    /// `Inst::InterfaceHasTag` answers a confident FALSE for a cast .NET performs.
    ///
    /// **NEITHER "ALWAYS OVERRIDE" NOR "FILL ONLY WHEN ABSENT" IS THE RULE, AND EACH IS WRONG ON A
    /// PROGRAM C# CAN WRITE.** An inherited row must beat the implicit entry for
    /// `class B : IFoo { int IFoo.Bar() {...} public virtual int Bar() {...} }` seen through
    /// `class D : B`, because the implicit pass matches `D`'s override by name and II.12.2 says `B`'s
    /// explicit mapping still governs. It must NOT beat it for `class D : B, IFoo` with a public
    /// `Bar()`, where `D` RE-IMPLEMENTS `IFoo` and .NET calls `D`'s. The two differ only in whether a
    /// more derived type DECLARES the interface, which is what `reimplemented` tracks.
    ///
    /// Two limits, both narrower than they look. Re-implementation is compared by the interface's
    /// FULL NAME, so a type re-implementing `IFoo<int>` also masks a base's `IFoo<string>` rows --
    /// the tag still distinguishes the two instantiations, so their entries cannot collide; only the
    /// mask is coarse. And an inherited row resolves to the BASE's body, which is what every shape C#
    /// can emit needs: csc marks an explicit implementation `private final virtual`, so no derived
    /// type can override it.
    ///
    ///
    /// `body_impl` is the half that genuinely differs between callers and is the only reason this takes
    /// a closure: a body in THIS module is a function index, and a body in a REFERENCED assembly is an
    /// extern symbol the linker resolves. The chain walk, the tag derivation and the override order are
    /// the parts that must not differ, so they live here rather than at each call site.
    fn fold_explicit_itable_entries(
        &self,
        links: &[(&'a Assembly<'a>, TypeDef<'a>)],
        body_impl: &dyn Fn(&'a Assembly<'a>, TypeDef<'a>, Token) -> Option<VtableEntry>,
        entries: &mut Vec<(u32, VtableEntry)>,
    ) {
        let mut reimplemented: Vec<String> = Vec::new();
        for &(assembly, type_def) in links {
            for (body, declaration) in type_def.method_impls() {
                let Some((interface, tag)) = self.explicit_interface_dispatch(assembly, declaration)
                else {
                    continue;
                };
                if reimplemented.iter().any(|name| *name == interface) {
                    continue;
                }
                let Some(implementation) = body_impl(assembly, type_def, body) else {
                    continue;
                };
                match entries.iter_mut().find(|(other, _)| *other == tag) {
                    Some(slot) => slot.1 = implementation,
                    None => entries.push((tag, implementation)),
                }
            }
            for token in type_def.interfaces() {
                if let Some(name) = assembly.type_token_name(token) {
                    reimplemented.push(joined_full_name(&name));
                }
            }
        }
    }

    /// One `InterfaceImpl` row's interface token resolved to the assembly that declares the
    /// interface, its `TypeDef`, and the [`InterfaceIdentity`] its tags fold.
    ///
    /// **THIS IS THE ONLY PLACE THAT DECIDES WHETHER AN INTERFACE'S TAG IDENTITY IS ITS SIMPLE NAME
    /// OR ITS CANONICAL INSTANTIATION SPELLING.** The itable side and the call site must derive the
    /// same value from different starting points, and the decision is the part that has to agree --
    /// so it is asked once here rather than at each site, where a fifth site would be free to reach
    /// a fourth answer.
    ///
    /// `arguments` are the INSTANTIATION's, and they matter only for a `TypeSpec` token:
    /// `class Box<T> : IBox<T>` records `` IBox`1<!0> `` in its row, and `Box<int>`'s itable has to
    /// key `` IBox`1[System.Int32] ``. An ordinary type passes an empty list, where substitution
    /// leaves a closed signature untouched -- which is what keeps every non-generic image on
    /// exactly the bytes it was on, and it is why `class C : IBox<int>` (a NON-generic type
    /// implementing a closed generic interface) resolves here too.
    ///
    /// `None` for a token this assembly cannot resolve, which SKIPS the interface rather than
    /// keying an entry by a fabricated identity.
    fn interface_link(
        &self,
        link_assembly: &'a Assembly<'a>,
        iface_token: Token,
        arguments: &[SigType],
    ) -> Option<(&'a Assembly<'a>, TypeDef<'a>, InterfaceIdentity<'a>)> {
        match iface_token.table() {
            table::TYPE_DEF => {
                let type_def = link_assembly.type_def(iface_token.row())?;
                let name = link_assembly.type_token_name(iface_token)?;
                Some((link_assembly, type_def, InterfaceIdentity::Named(name)))
            }
            table::TYPE_REF => {
                let name = link_assembly.type_token_name(iface_token)?;
                let (_, owner, type_def) = self.find_reference_type(name.namespace, name.name)?;
                let owner_name = owner.type_token_name(type_def.token())?;
                Some((owner, type_def, InterfaceIdentity::Named(owner_name)))
            }
            table::TYPE_SPEC => {
                let signature = link_assembly.type_spec_signature(iface_token)?;
                let closed = crate::generics::substitute_sig(&signature, arguments)?;
                let SigType::GenericInst { definition, .. } = &closed else {
                    return None;
                };
                let definition_token = match definition.as_ref() {
                    SigType::Class(token) | SigType::ValueType(token) => *token,
                    _ => return None,
                };
                let name = link_assembly.type_token_name(definition_token)?;
                let (owner, type_def) = if definition_token.table() == table::TYPE_DEF {
                    (
                        link_assembly,
                        link_assembly.type_def(definition_token.row())?,
                    )
                } else {
                    let (_, owner, type_def) =
                        self.find_reference_type(name.namespace, name.name)?;
                    (owner, type_def)
                };
                let identity = InterfaceIdentity::instantiated(link_assembly, &closed)?;
                Some((owner, type_def, identity))
            }
            _ => None,
        }
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
        for type_def in self.descriptor_type_defs() {
            let impls = self.vtable_methods(type_def);
            let entries = self.interface_entries(self.assembly, type_def, &impls, &[], &|rid| {
                self.function_index(rid)
            });
            if !entries.is_empty() {
                result.push((TypeHandle(type_def.token().0), entries));
            }
        }
        result
    }

    /// ONE type's itable entries: `(interface_method_tag, implementation)` for every interface
    /// method it answers for, implicit and explicit alike, with `impls` its vtable slots as
    /// [`vtable_methods`](Self::vtable_methods) numbered them.
    ///
    /// **`resolve` MAPS A `MethodDef` RID TO THE FUNCTION INDEX THAT IMPLEMENTS IT, AND IT IS A
    /// PARAMETER BECAUSE THAT IS THE ONLY THING AN INSTANTIATION DOES DIFFERENTLY.** An ordinary
    /// type's slot is its own [`function_index`](Self::function_index); an instantiation's is the
    /// MONOMORPHIZED body the plan emits for `(this instantiation, this method)`. Everything else --
    /// the interface closure, the by-name tag, the `MethodImpl` override -- is one rule, so a
    /// correction to it reaches both callers instead of one.
    ///
    /// `arguments` are the instantiation's, forwarded to [`Self::interface_link`] so a GENERIC
    /// interface keys by the spelling of its own closed instantiation. An ordinary type passes an
    /// empty list.
    ///
    /// `assembly` IS THE ONE `type_def`'s ROWS LIVE IN, and it is a parameter for the same reason
    /// `resolve` is: an instantiation of a definition declared in a REFERENCED assembly reads its
    /// `InterfaceImpl` rows, its `MethodImpl` rows and its method names out of the OWNER's tables,
    /// because that is the only assembly those rows index. Passing `self.assembly` for such a type
    /// would read whatever happens to sit at those row numbers here -- a real, unrelated, plausible
    /// interface.
    /// Every `(dispatch tag, implementing slot key)` ONE interface method contributes: a single pair
    /// for an ordinary method, and one PER INSTANTIATION for a generic one.
    ///
    /// # The two halves are derived side by side because they must agree pairwise
    ///
    /// The tag is what a `callvirt` puts in the emitted itable scan; the key is what finds the
    /// implementing vtable slot. Building the two lists in separate loops is how entry *i*'s tag
    /// comes to sit beside entry *j*'s implementation -- a dispatch that resolves, links, runs, and
    /// calls `Tag<string>`'s body for `Tag<int>`.
    ///
    /// # Which assembly reads which, and why they differ
    ///
    /// The PARAMETERS are read against the interface's own assembly, so the interface's signature
    /// and the implementor's compare equal across a boundary -- `param_key`'s existing rule. The
    /// ARGUMENTS are the calling module's, spelled against `self.assembly`, because that is where
    /// the `MethodSpec` rows they were decoded from live.
    fn interface_method_keys(
        &self,
        iface_assembly: &Assembly<'_>,
        iface_name: &TypeName,
        name: &str,
        signature: &lamella_metadata::MethodSig,
    ) -> Vec<(u32, String)> {
        let params = &signature.parameters;
        let base = param_key(iface_assembly, signature.generic_param_count, params);
        if signature.generic_param_count == 0 {
            return match interface_method_tag(iface_assembly, iface_name, name, params) {
                Some(tag) => alloc::vec![(tag, base)],
                None => Vec::new(),
            };
        }
        self.mono
            .virtual_method_instantiations(name, signature.generic_param_count, params)
            .into_iter()
            .filter_map(|arguments| {
                let tag = instantiated_interface_method_tag(
                    iface_assembly,
                    iface_name,
                    name,
                    params,
                    self.assembly,
                    arguments,
                )?;
                Some((tag, instantiated_slot_key(self.assembly, &base, arguments)))
            })
            .collect()
    }

    fn interface_entries(
        &self,
        assembly: &'a Assembly<'a>,
        type_def: TypeDef<'a>,
        impls: &[VSlot<'a>],
        arguments: &[SigType],
        resolve: &dyn Fn(u32) -> Option<u32>,
    ) -> Vec<(u32, VtableEntry)> {
        {
            let mut entries: Vec<(u32, VtableEntry)> = Vec::new();
            for iface_token in self.interface_closure(assembly, type_def) {
                let Some((iface_assembly, iface, identity)) =
                    self.interface_link(assembly, iface_token, arguments)
                else {
                    continue;
                };
                let iface_name = identity.type_name();
                for method in iface.methods() {
                    let Some(name) = method.name() else { continue };
                    let Some(signature) = decodable_signature(&method) else {
                        continue;
                    };
                    for (tag, key) in
                        self.interface_method_keys(iface_assembly, &iface_name, name, &signature)
                    {
                        let Some(slot) = impls
                            .iter()
                            .find(|slot| slot.name == Some(name) && slot.key == key)
                        else {
                            continue;
                        };
                        if let Some(func_index) = module_slot_index(slot, resolve) {
                            entries.push((tag, VtableEntry::Func(func_index)));
                        }
                    }
                }
            }
            let chain: Vec<(&'a Assembly<'a>, TypeDef<'a>)> = assembly_base_chain(assembly, type_def)
                .into_iter()
                .map(|td| (assembly, td))
                .collect();
            self.fold_explicit_itable_entries(
                &chain,
                &|link, _, body| {
                    let MethodKind::Definition(rid) = link.resolve_method(body)?.kind else {
                        return None;
                    };
                    Some(VtableEntry::Func(resolve(rid)?))
                },
                &mut entries,
            );
            entries
        }
    }
}

/// The identity an interface contributes to an [`interface_method_tag`], in the one canonical form.
///
/// **A NON-GENERIC interface contributes its `(namespace, simple name)` pair, byte for byte as it
/// always has.** A GENERIC one contributes its whole canonical instantiation spelling in the NAME
/// half with the namespace EMPTY.
///
/// **THE EMPTY NAMESPACE IS LOAD-BEARING RATHER THAN TIDY.** A spelled instantiation already
/// carries the full dotted name of every part -- `System.Collections.Generic.IList`1[System.Int32]`
/// -- because [`crate::generics::spell_sig`] names each argument through the definition's full
/// name. Folding the definition's namespace in FRONT of that spells one identity twice, and the
/// result would still be injective, which is exactly what makes it worth refusing explicitly: it
/// would be a second spelling of an identity that is baked into emitted code.
///
/// The two populations cannot collide for a structural reason rather than a probabilistic one:
/// neither a namespace nor a compiler-produced type name can contain a `[`, so a generic tag's
/// byte stream is distinguishable from every non-generic one by inspection.
enum InterfaceIdentity<'a> {
    /// The `(namespace, name)` pair of an interface named directly by a `TypeDef` or `TypeRef`.
    Named(TypeName<'a>),
    /// The canonical spelling of a generic interface's own instantiation.
    Instantiated(String),
}

impl InterfaceIdentity<'_> {
    /// The identity of a GENERIC interface named by a CLOSED instantiation signature.
    ///
    /// **BOTH HALVES OF DISPATCH COME THROUGH HERE**, from opposite directions: the itable side
    /// closes an `InterfaceImpl` row's signature with the implementing instantiation's arguments,
    /// and the call site closes the `TypeSpec` its `MemberRef` is parented by. They must produce
    /// the same bytes, so they spell through one function rather than two that agree today.
    ///
    /// `None` when the signature names a type this assembly cannot name, which REFUSES the tag --
    /// a caller must skip the member rather than key it by a fabricated identity.
    fn instantiated(assembly: &Assembly<'_>, closed: &SigType) -> Option<Self> {
        crate::generics::spell_sig(assembly, closed).map(Self::Instantiated)
    }

    /// The pair [`interface_method_tag`] folds.
    fn type_name(&self) -> TypeName<'_> {
        match self {
            Self::Named(name) => *name,
            Self::Instantiated(spelled) => TypeName {
                namespace: "",
                name: spelled,
            },
        }
    }
}

/// How a `callvirt` on a VIRTUAL GENERIC METHOD reaches the body that runs: through a vtable slot
/// keyed by the call site's type arguments.
///
/// It is an enum with one variant today and that is deliberate rather than premature. A method
/// declared on an INTERFACE dispatches through the itable by TAG and not by slot -- a different
/// emitted instruction, not a different number -- and the shape is cut and gated (`genvgeniface`).
/// Naming the axis now is what makes the interface arm a variant added to one judgement rather than
/// a second judgement written beside it, which is this tree's most-cited defect.
enum GenericDispatch {
    /// The receiver's vtable slot index -- a method declared on a CLASS.
    Slot(usize),
    /// The itable tag the receiver is scanned for -- a method declared on an INTERFACE, which
    /// occupies no vtable slot on the types that implement it.
    Tag(u32),
}

/// A type's vtable SLOTS as the ENTRIES a descriptor carries: each slot's implementation resolved
/// to a function index by `resolve`, an inherited referenced-assembly slot kept as its extern
/// symbol.
///
/// **`None` WHEN ANY SLOT DOES NOT RESOLVE, AND THE ALL-OR-NOTHING IS THE POINT.** A vtable with a
/// slot left out is not a smaller vtable -- every slot after it shifts, so a `callvirt` computed
/// against the numbering lands on a different method. The caller emits the whole table or none of
/// it.
fn slot_entries(
    slots: &[VSlot<'_>],
    resolve: &dyn Fn(u32) -> Option<u32>,
) -> Option<Vec<VtableEntry>> {
    slots.iter().map(|slot| slot_entry(slot, resolve)).collect()
}

/// The function index a slot names WHEN THE IMPLEMENTATION IS THIS MODULE'S, or `None` for one that
/// lives in a reference.
///
/// An itable entry carries a function INDEX, and a slot whose implementation is a referenced
/// assembly's has only that assembly's extern SYMBOL -- there is no index here to write down. So
/// this is the itable's half of [`slot_entry`], with the same job of giving a new [`SlotImpl`]
/// variant its case in ONE place.
fn module_slot_index(slot: &VSlot<'_>, resolve: &dyn Fn(u32) -> Option<u32>) -> Option<u32> {
    match &slot.impl_ {
        SlotImpl::Rid(rid) => resolve(*rid),
        SlotImpl::Mono(index) => Some(*index),
        SlotImpl::Extern(_) => None,
    }
}

/// ONE slot as the entry a descriptor carries.
///
/// **EVERY SITE THAT TURNS A [`VSlot`] INTO A [`VtableEntry`] GOES THROUGH HERE**, because the
/// mapping is exactly where a new [`SlotImpl`] variant gets its case in one implementation and not
/// in the others -- and a slot silently mapped by a stale arm is a table entry naming the wrong
/// function, which links.
fn slot_entry(slot: &VSlot<'_>, resolve: &dyn Fn(u32) -> Option<u32>) -> Option<VtableEntry> {
    match &slot.impl_ {
        SlotImpl::Rid(rid) => resolve(*rid).map(VtableEntry::Func),
        SlotImpl::Extern(symbol) => Some(VtableEntry::Extern(symbol.clone())),
        SlotImpl::Mono(index) => Some(VtableEntry::Func(*index)),
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
/// module function), a referenced-assembly method named by its stable extern symbol, or a
/// MONOMORPHIZED body the plan already numbered.
enum SlotImpl {
    Rid(u32),
    Extern(String),
    /// A FUNCTION INDEX, already resolved -- never a rid.
    ///
    /// **THE DISTINCTION IS THE WHOLE REASON THIS IS A THIRD VARIANT AND NOT A `Rid`.** A virtual
    /// generic method's body is planned past `max_rid` in the index space, so the number it carries
    /// is not a `MethodDef` row and must not be mapped through `function_index` -- doing so would
    /// look the index up in a rid table where it means nothing, and on the arms that fall back to
    /// the identity mapping it would silently name a different function that LINKS.
    Mono(u32),
}

/// A method's decoded parameter list, or `None` when its signature CANNOT BE DECODED -- today that
/// means a generic one. [`lamella_metadata::parse_method`] refuses those deliberately, because a
/// generic signature carries an extra leading `GenParamCount` and reading past it would yield a
/// plausible and WRONG `MethodSig` with no error at all.
///
/// **THE WHOLE VALUE OF THIS FUNCTION IS THAT IT HAS NO DEFAULT.** The reader maps that refusal to
/// `None`, and turning that `None` into an EMPTY parameter list admits an undecodable method with a
/// fabricated arity of zero. That is not cosmetic. The list feeds [`extern_method_symbol`], a
/// CROSS-ASSEMBLY symbol name, so two overloads collapse onto one; it feeds [`param_key`], so a
/// generic `M<T>()` can be selected as the override for `M()`; and [`interface_method_tag`], a value
/// no later build may re-spell, is computed from the same shape.
///
/// **A caller must SKIP the member, never substitute.** Present-but-unreadable is not absent, and it
/// is certainly not nullary. Undecodable methods are the ORDINARY case for a real csc-produced
/// assembly rather than an exotic one -- on the .NET 8 reference assemblies they run to a fifth of
/// `System.Runtime` and nearly all of `System.Linq`.
///
/// The sibling fix in `lamella-binder` (`LAM0002`, `e29c813709`) does NOT cover this crate:
/// `lamella-aot` has no dependency on `lamella-binder`, direct or transitive, and reaches
/// `lamella-metadata` itself.
#[must_use]
pub fn decodable_params(method: &lamella_metadata::Method<'_>) -> Option<Vec<SigType>> {
    decodable_signature(method).map(|sig| sig.parameters)
}

/// A method's decoded SIGNATURE, on the same terms [`decodable_params`] documents: `None` when the
/// signature cannot be decoded, and a caller SKIPS the member rather than substituting a default.
/// [`extern_method_symbol`] needs the return type as well as the parameters -- the return type is part
/// of the CLI signature, and conversion operators overload on it -- so a caller that wants both takes
/// them from one decode instead of asking twice.
#[must_use]
pub fn decodable_signature(
    method: &lamella_metadata::Method<'_>,
) -> Option<lamella_metadata::MethodSig> {
    method.signature()
}

/// The assembly-independent identity of a method's signature for DISPATCH MATCHING: its generic
/// ARITY, then each parameter's extern-symbol encoding (a primitive one char, a class/value type its
/// FULL NAME), so a referenced base's signature and a this-assembly override's compare equal even
/// though their `SigType` tokens index different metadata tables.
///
/// # THE ARITY IS REQUIRED, AND IT IS A PARAMETER SO THAT THE COMPILER ENUMERATES THE SITES
///
/// `int Tag()` and `int Tag<T>()` are a LEGAL C# overload pair with the same name and the same
/// (empty) parameter list. Keyed on parameters alone they encode identically, so the numbering walk
/// selects `Tag<T>`'s override as the implementation of `Tag`'s slot and a plain `b.Tag()` runs the
/// generic body. **That is a wrong answer with no error and no violation, in ORDINARY virtual
/// dispatch, reachable by a program whose only use of generics is declaring a method nobody calls.**
/// Both tiers reached the collision from the same direction, so the arity is part of the key in both.
///
/// ```text
/// ECMA-334 14.4.2   overload resolution considers generic arity as well as parameter types
/// ECMA-335 II.9.9   the number of generic parameters shall match exactly when overriding
/// ```
///
/// **BOTH SIDES OF A DISPATCH MUST COMPUTE THE SAME KEY**, and the two call sites reach it from
/// opposite directions -- a slot's declaring method, and an interface method being matched to its
/// implementation. A site that could not reach the arity and passed `0` to compile would keep the
/// collision on that path alone, invisible because the other looked fixed. Making it required
/// rather than defaulted is what turns that into a compile error instead of a silent one; both
/// sites hold a decoded [`MethodSig`], which carries `generic_param_count` from II.23.2.1's
/// `GENERIC` convention, so both can answer.
///
/// **THIS IS A COMPARISON KEY AND NOT AN ABI, AND THAT IS MEASURED RATHER THAN ARGUED.** It is never
/// serialized: the link name is [`extern_method_symbol`] and the interface dispatch tag is
/// [`interface_method_tag`], both of which a later build must reproduce byte for byte. Neither is
/// computed from this.
///
/// The measurement is a comparison-preserving perturbation, which is the only kind that separates
/// "not serialized" from "serialized but stable": prefixing every key with a constant changes every
/// key STRING while preserving every equality, so a key that reached a symbol or a tag would move
/// image bytes, and one that does not cannot.
///
/// **The key may therefore GAIN A COMPONENT without touching the frozen encoding.** What has to
/// agree between two implementations of a dispatch is the resulting slot ORDER, which follows from
/// both including a component at all -- never from spelling it alike.
fn param_key(assembly: &Assembly, generic_arity: u32, params: &[SigType]) -> String {
    let mut key = alloc::format!("{generic_arity}#");
    for p in params {
        encode_type(
            p,
            &|token| assembly.type_token_name(token).map(|n| joined_full_name(&n)),
            &mut key,
        );
    }
    key
}

/// A virtual method's slot key: its [`param_key`], or -- when the signature does not decode -- a key
/// **nothing else can match**.
///
/// # Why neither a skip nor an empty list
///
/// A vtable is POSITIONAL. **Skipping an undecodable method removes a SLOT and slides every later
/// index down**, so a `callvirt` compiled against one numbering lands on a different body under the
/// other; that is the one outcome worse than the defect. And a defaulted EMPTY parameter list is the
/// defect: a nullary `M()` and an undecodable `M(List<int>)` would key IDENTICALLY, so the second is
/// selected as the override for the first -- the wrong-bind [`decodable_params`] exists to prevent,
/// reached through a different door.
///
/// **So the method keeps its slot and matches nothing.** `#<rid>` cannot be a real key: a decoded one
/// begins with its ARITY, hence with a digit, and this begins with `#`. It does not match the same
/// method read from another assembly either, which is correct rather than unfortunate -- **a
/// signature we cannot read is not one we can pair across a boundary**, and pairing it by name alone
/// is exactly how two overloads collapse.
///
/// **THE ARM IS UNREACHABLE OVER WELL-FORMED METADATA AND THE GUARD IS STILL RIGHT.** Generic
/// signatures decode, and they were the whole undecodable population on the .NET 8 reference
/// assemblies. What this covers is a TRUNCATED OR MALFORMED signature blob, which still refuses.
/// A virtual GENERIC method's slot key at one instantiation: its ordinary [`param_key`] with the
/// call site's type arguments appended.
///
/// **ONE FUNCTION BECAUSE THE TWO SIDES OF A DISPATCH MEET HERE.** The numbering walk builds the
/// key while laying the table; the `callvirt` builds it again while computing the index to jump
/// through. Spelling it in two places is how the two come to disagree, and a disagreement here is
/// not an error -- it is a `callvirt` that finds no slot and silently falls back to a direct call
/// on the BASE's body, which links, runs and answers a plausible wrong number.
///
/// The arguments go through [`crate::generics::spell_sig`], this tree's ONE speller, for the same
/// reason: a second encoding of a type argument is a second answer to "are these the same
/// instantiation". An argument that will not spell folds to `?`, which cannot collide with a real
/// spelling and makes the two sides agree that this instantiation has no slot.
fn instantiated_slot_key(assembly: &Assembly, key: &str, arguments: &[SigType]) -> String {
    let spelled: Vec<String> = arguments
        .iter()
        .map(|argument| {
            crate::generics::spell_sig(assembly, argument).unwrap_or_else(|| String::from("?"))
        })
        .collect();
    alloc::format!("{key}<{}>", spelled.join(","))
}

fn slot_key(assembly: &Assembly, method: &lamella_metadata::Method<'_>, rid: u32) -> String {
    match decodable_signature(method) {
        Some(sig) => param_key(assembly, sig.generic_param_count, &sig.parameters),
        None => alloc::format!("#{rid}"),
    }
}

/// The `extends` chain of `type_def` WITHIN `assembly`, derived-first (self at index 0), stopping at
/// a base this assembly does not declare (nil for `System.Object`, or a TypeRef into another
/// assembly). Bounded against a malformed cyclic `extends`.
///
/// # A `TypeSpec` base continues the chain, and it carries NO arguments here
///
/// `class Derived<T> : Base<T>` spells its base as an instantiation, and this walk exists to
/// NUMBER SLOTS. A slot's key is its declaring method's name and its OPEN parameter signature --
/// the same for `Base<int>` and `Base<string>` -- so the numbering needs the base's DEFINITION and
/// nothing about its arguments. Following the `TypeSpec` to that definition is therefore the whole
/// change, and the layout walk ([`MetadataResolver::cross_class_chain`]) is where the arguments
/// have to travel.
///
/// **A CHAIN THAT STOPS EARLY IS NOT A SHORTER ANSWER, IT IS A WRONG ONE REPORTED AS COMPLETE.**
/// Every caller numbers against what this returns, so a base left out is a table missing every slot
/// that base declares -- and a `callvirt` through a `Base<int>`-typed reference computes its index
/// in the base's numbering and lands past the end of the derived type's table. There is no
/// signal: the walk cannot distinguish "reached the top" from "could not go further", which is why
/// an unfollowable base ends it rather than being skipped over.
fn assembly_base_chain<'x>(assembly: &'x Assembly<'x>, type_def: TypeDef<'x>) -> Vec<TypeDef<'x>> {
    let mut chain = Vec::new();
    let mut current = Some(type_def);
    for _ in 0..64 {
        let Some(td) = current else {
            break;
        };
        chain.push(td);
        let base = td.extends();
        current = match base.table() {
            table::TYPE_DEF if base.row() != 0 => assembly.type_def(base.row()),
            table::TYPE_SPEC => generic_base_definition(assembly, base)
                .filter(|token| token.table() == table::TYPE_DEF)
                .and_then(|token| assembly.type_def(token.row())),
            _ => None,
        };
    }
    chain
}

/// Whether a CLOSED signature denotes a REFERENCE type -- decided from the signature's own element
/// encoding and never from a fallback.
///
/// **THE ENCODING ITSELF ANSWERS THIS, WHICH IS THE ONLY REASON IT IS SAFE TO ASK.** ECMA-335
/// II.23.1.16 gives `ELEMENT_TYPE_CLASS` and `ELEMENT_TYPE_VALUETYPE` distinct bytes, so a `Class`
/// is a reference type by construction rather than by resolving the token and finding no value-type
/// layout. That distinction matters because the two failure directions are not symmetric: judging a
/// reference type a VALUE type boxes a pointer into a fresh object -- wrong, and loud when the cast
/// back fails -- while judging a value type a REFERENCE type skips the allocation and leaves raw
/// bytes where an object reference is expected, which is a memory-safety defect. Anything this
/// cannot prove is a reference type therefore answers `false`.
fn is_reference_signature(sig: &SigType) -> bool {
    match sig {
        SigType::Class(_) | SigType::String | SigType::Object | SigType::SzArray(_) => true,
        SigType::Array { .. } => true,
        SigType::GenericInst { definition, .. } => {
            matches!(definition.as_ref(), SigType::Class(_))
        }
        _ => false,
    }
}

/// ONE link of a class chain: the assembly its rows live in, the type, and the type ARGUMENTS in
/// force for it.
///
/// **THE ARGUMENTS ARE PER-LINK BECAUSE A GENERIC BASE HAS ITS OWN.** `Derived<int> : Base<T>` lays
/// `Base`'s block under `[int]` and `Derived`'s under `[int]` too, but `Derived<int> : Base<string>`
/// is legal and lays them under different lists -- so one argument list for the whole chain is a
/// shape that happens to be right for the common case and silently wrong for the general one.
/// EMPTY means the link is not generic and lays out the ordinary way.
struct ChainLink<'a> {
    assembly: &'a Assembly<'a>,
    type_def: TypeDef<'a>,
    arguments: Vec<SigType>,
}

/// The chain in the order every caller reads it: BASE-FIRST, `System.Object`-most ancestor at index
/// 0 and the type itself last. One function so an early stop and a completed walk cannot differ in
/// their ordering -- a reversed partial chain lays the blocks in the wrong order rather than
/// omitting them, which is a wrong offset instead of a missing field.
fn finish(mut chain: Vec<ChainLink<'_>>) -> Vec<ChainLink<'_>> {
    chain.reverse();
    chain
}

/// The DEFINITION token a `TypeSpec` base names -- `` Base`1 `` for a base spelled `Base<T>`.
///
/// `None` for a `TypeSpec` that is not a generic instantiation at all (an array or pointer base is
/// not expressible in `extends`, but the signature reader does not owe us that) and for one whose
/// definition is not a type token. Shared by the numbering walk and the layout walk so the two
/// cannot disagree about which type a generic base IS.
fn generic_base_definition(assembly: &Assembly<'_>, base: Token) -> Option<Token> {
    let SigType::GenericInst { definition, .. } = assembly.type_spec_signature(base)? else {
        return None;
    };
    match definition.as_ref() {
        SigType::Class(token) | SigType::ValueType(token) => Some(*token),
        _ => None,
    }
}

/// Whether `type_def` is a delegate type, judged within its OWN `assembly` (the program's, or the
/// referenced corlib's for a cross-assembly `new ThreadStart(...)`): its `extends` chain reaches
/// `System.MulticastDelegate`/`System.Delegate`. The walk is bounded so a malformed cyclic base
/// cannot loop.
pub(crate) fn is_delegate_type_of<'x>(assembly: &'x Assembly<'x>, type_def: &TypeDef<'x>) -> bool {
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
            let key = slot_key(assembly, &method, method.rid());
            let sig = decodable_signature(&method);
            let params = sig
                .as_ref()
                .map(|sig| sig.parameters.clone())
                .unwrap_or_default();
            let return_type = sig.map(|sig| sig.return_type).unwrap_or(SigType::Void);
            let symbol = extern_method_symbol(
                &owner_namespace,
                &owner_name,
                name.unwrap_or(""),
                &params,
                &return_type,
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
        for link in self.cross_class_chain(owner, type_def, &[]) {
            let layout = self.link_layout(&link)?;
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
        arguments: &[SigType],
    ) -> Vec<ChainLink<'a>> {
        let mut assembly = owner;
        let mut chain = alloc::vec![ChainLink {
            assembly,
            type_def,
            arguments: arguments.to_vec(),
        }];
        let mut current = type_def.extends();
        let mut in_force: Vec<SigType> = arguments.to_vec();
        for _ in 0..64 {
            if current.row() == 0 {
                break;
            }
            let (base, base_arguments) = match current.table() {
                table::TYPE_DEF => match assembly.type_def(current.row()) {
                    Some(base) => (base, Vec::new()),
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
                    (base, Vec::new())
                }
                table::TYPE_SPEC => {
                    let Some(SigType::GenericInst {
                        definition,
                        arguments: spelled,
                    }) = assembly.type_spec_signature(current)
                    else {
                        break;
                    };
                    let mut composed = Vec::new();
                    for argument in &spelled {
                        match crate::generics::substitute_sig(argument, &in_force) {
                            Some(closed) => composed.push(closed),
                            None => return finish(chain),
                        }
                    }
                    let (SigType::Class(token) | SigType::ValueType(token)) = definition.as_ref()
                    else {
                        break;
                    };
                    let resolved = if token.table() == table::TYPE_DEF {
                        assembly.type_def(token.row())
                    } else {
                        match assembly
                            .type_token_name(*token)
                            .and_then(|name| self.find_reference_type(name.namespace, name.name))
                        {
                            Some((_, base_owner, base)) => {
                                assembly = base_owner;
                                Some(base)
                            }
                            None => None,
                        }
                    };
                    match resolved {
                        Some(base) => (base, composed),
                        None => break,
                    }
                }
                _ => break,
            };
            in_force = base_arguments.clone();
            chain.push(ChainLink {
                assembly,
                type_def: base,
                arguments: base_arguments,
            });
            current = base.extends();
        }
        finish(chain)
    }

    /// Type arguments read off a `TypeSpec`, with the instantiation IN FORCE applied to each.
    ///
    /// With no instantiation in force this is the identity, which is what keeps every ordinary
    /// program on exactly the bytes it was on. Under one it turns `` Box`1<!0> `` into
    /// `` Box`1<System.Int32> `` while lowering `Box<int>`'s copy of the body -- the composition
    /// that makes a definition able to talk about itself.
    ///
    /// `None` when an argument does not close: a parameter with no argument, or a method
    /// parameter (`!!n`) whose argument comes from a `MethodSpec` at the call site. A type that is
    /// not closed must not be layout-able, so this refuses rather than passing the parameter
    /// through to be sized as whatever the layout code makes of it.
    fn close_arguments(&self, arguments: Vec<SigType>) -> Option<Vec<SigType>> {
        if self.type_arguments.is_empty() {
            return Some(arguments);
        }
        arguments
            .iter()
            .map(|argument| crate::generics::substitute_sig(argument, &self.type_arguments))
            .collect()
    }

    /// A `TypeSpec`'s own signature with the instantiation in force applied -- the form to SPELL,
    /// because a spelling is an IDENTITY and `` Box`1[!0] `` is not one.
    ///
    /// It is [`Self::close_arguments`] one level out: that one closes the arguments a consumer
    /// substitutes WITH, this one closes the whole constructed type a consumer NAMES.
    fn closed_spec_signature(&self, spec: Token) -> Option<SigType> {
        let signature = self.assembly.type_spec_signature(spec)?;
        if self.type_arguments.is_empty() {
            return Some(signature);
        }
        crate::generics::substitute_sig(&signature, &self.type_arguments)
    }

    /// What a TYPE OPERAND denotes once this resolver's instantiation is applied, for the ONE operand
    /// shape whose meaning changes under one: a `TypeSpec`. `None` for every other token and for a
    /// body with no instantiation in force, which leaves each caller on the path it was already on.
    ///
    /// **ANSWERED ONCE, HERE, RATHER THAN AT EACH OF THE THREE CALL SITES.**
    /// [`CallResolver::array_element`], [`CallResolver::type_operand_mir`] and
    /// [`CallResolver::boxed_value_type`] each ask a different question about the same operand, and
    /// each starts from [`Assembly::type_token_name`] -- which a `TypeSpec` has none of, so each
    /// would otherwise fall past every arm to a default: four bytes and OPAQUE, `ObjectRef`, and a
    /// layout miss respectively. They consult this before their own name-based matches.
    ///
    /// **THE LAYOUT LIST, through [`Self::apply_instantiation`], and not the identity one**, because
    /// every consumer of this sizes a slot or types a value and none of them spells a name -- an ENUM
    /// argument is its underlying integer here and emphatically not `System.Int32`.
    fn closed_operand_sig(&self, token: Token) -> Option<SigType> {
        if token.table() != table::TYPE_SPEC {
            return None;
        }
        if self.layout_arguments.is_empty() && self.method_arguments.is_empty() {
            return None;
        }
        let signature = self.assembly.type_spec_signature(token)?;
        let closed = self.apply_instantiation(&signature)?;
        (closed != signature).then_some(closed)
    }

    /// The element of `new T[n]` where the operand is a `TypeSpec` and an instantiation is in force:
    /// the closed ARGUMENT's identity, size and kind. `None` leaves [`CallResolver::array_element`]
    /// on the path it was already on, which is what keeps every ordinary program on exactly the bytes
    /// it was on.
    ///
    /// **A CLOSED FORM THAT NAMES NO ROW IS THE WHOLE BCL CASE.** A primitive or `String` is spelled
    /// by a byte in the signature encoding and carries no token from anywhere, so naming it is a
    /// lookup and not a rebase -- `List<int>` and `List<string>` are exactly this.
    ///
    /// **A NAMED ARGUMENT -- a class or struct the CALLER declares -- IS ALSO TAKEN, THROUGH THE
    /// ARGUMENT WORLD.** Its token belongs to the caller while this resolver reads the owner, which
    /// is the problem [`marked_handle_token`] and [`value_type_layout_across`] solve for the same
    /// operand in [`mir_type_across`]; sharing that pair is what stops the array's stride and the
    /// element's MIR type being decided by two different rules. Three of this function's four
    /// outputs were wrong for such an argument while only the identity refused: a struct strode by
    /// `unwrap_or(4)` where the signature beside it said eight, and a class element was described
    /// OPAQUE, which tells a mark-compact collector not to scan an array full of live references.
    ///
    /// **ONE SHAPE STILL DECLINES, AND IT IS A PROPERTY OF THE DESCRIPTOR RATHER THAN OF THIS
    /// FUNCTION:** a struct element holding REFERENCE FIELDS. Word 1 carries a kind and no
    /// per-element offsets, so such an array cannot be described at all -- see the arm for the
    /// reasoning. An unsubstituted spec declines too: `new int[][]` and `new Box<int>[n]` close to
    /// an array and an instantiation, neither of which this names.
    ///
    /// The identity is resolved OWN-ASSEMBLY FIRST, exactly as [`resolve_value_type_def`] resolves a
    /// value type, and for the same reason: a body lowered out of the corlib is read against the
    /// corlib itself with NO references attached ([`Self::rebased_on_reference`] keeps only the
    /// ordinals BELOW its own), so `System.Int32` is a `TypeDef` there and a reference lookup would
    /// miss it. The own-assembly handle it yields is then rebased by `build::rebase_identities` like
    /// any other, which is what makes this array's descriptor the SAME one a program-side
    /// `new int[n]` names rather than a second copy of it.
    fn substituted_array_element(&self, spec: Token) -> Option<ArrayElement> {
        let closed = self.closed_operand_sig(spec)?;
        if let Some((namespace, name)) = primitive_sig_name(&closed) {
            let element = self
                .assembly
                .find_type(namespace, name)
                .map(|type_def| TypeHandle(type_def.token().0))
                .or_else(|| {
                    self.find_reference_type(namespace, name)
                        .map(|(ordinal, _, type_def)| reference_handle(ordinal, type_def.token().0))
                })?;
            return Some(ArrayElement {
                handle: lamella_ir::array_handle(element),
                element: Some(element),
                element_size: primitive_value_size(namespace, name).unwrap_or(4),
                element_kind: primitive_element_kind(namespace, name)
                    .unwrap_or(ELEMENT_KIND_REFERENCE),
            });
        }
        match &closed {
            SigType::Class(token) => {
                let element = TypeHandle(
                    match self.argument_assembly {
                        Some(_) => in_argument_world(*token),
                        None => *token,
                    }
                    .0,
                );
                Some(ArrayElement {
                    handle: argument_world_array_handle(element),
                    element: Some(element),
                    element_size: 4,
                    element_kind: ELEMENT_KIND_REFERENCE,
                })
            }
            SigType::ValueType(token) => {
                let layout = value_type_layout_across(
                    self.assembly,
                    self.argument_assembly,
                    *token,
                    self.references(),
                    &TargetLayout::ilp32(),
                )?;
                if !layout.reference_offsets.is_empty() {
                    return None;
                }
                let element = TypeHandle(marked_handle_token(*token, self.argument_assembly).0);
                Some(ArrayElement {
                    handle: argument_world_array_handle(element),
                    element: Some(element),
                    element_size: layout.size,
                    element_kind: ELEMENT_KIND_OPAQUE,
                })
            }
            _ => None,
        }
    }

    /// A `TypeSpec` parent resolved to the generic definition it instantiates and the arguments to
    /// substitute: `(owning assembly, the definition's TypeDef, the type arguments)`.
    ///
    /// The definition is found BY NAME, through the same [`Self::find_reference_type`] every other
    /// cross-assembly lookup uses -- a `TypeSpec`'s definition token may be a `TypeRef` into corlib,
    /// and a token is meaningless outside its own assembly.
    fn instantiated_parent(
        &self,
        token: Token,
    ) -> Option<(&'a Assembly<'a>, TypeDef<'a>, Vec<SigType>)> {
        let SigType::GenericInst {
            definition,
            arguments,
        } = self.assembly.type_spec_signature(token)?
        else {
            return None;
        };
        let arguments = self.close_arguments(arguments)?;
        let definition = match definition.as_ref() {
            SigType::Class(token) | SigType::ValueType(token) => *token,
            _ => return None,
        };
        let name = self.assembly.type_token_name(definition)?;
        match definition.table() {
            table::TYPE_DEF => Some((
                self.assembly,
                self.assembly.type_def(definition.row())?,
                arguments,
            )),
            _ => {
                let (_, owner, type_def) = self.find_reference_type(name.namespace, name.name)?;
                Some((owner, type_def, arguments))
            }
        }
    }

    /// The layout of one INSTANTIATION of a generic definition: the definition's own instance
    /// fields with `!n` replaced by the instantiation's arguments, then laid out.
    ///
    /// **THIS IS WHY MONOMORPHIZATION EXISTS, IN ONE FUNCTION.** `Box<int>` has a four-byte
    /// payload and NO reference offsets; `Box<string>` has a four-byte payload and ONE reference at
    /// offset 0. The GC trace map differs, so the two cannot share a descriptor however alike their
    /// sizes look -- and a resolver that ignored the type argument would hand both the same map and
    /// leave the collector blind to one of them.
    ///
    /// **The layout itself is NOT recomputed here.** The field types are substituted and handed to
    /// the same [`layout_value_type`] every non-generic type goes through, so packing, alignment and
    /// the reference map cannot drift between a generic type and an ordinary one.
    fn instantiated_layout(
        &self,
        owner: &'a Assembly<'a>,
        type_def: TypeDef<'a>,
        arguments: &[SigType],
    ) -> Option<TypeLayout> {
        let mut fields = Vec::new();
        for field in type_def.fields().filter(|field| !field.is_static()) {
            fields.push(crate::generics::substitute_sig(
                &field.signature()?,
                arguments,
            )?);
        }
        layout_value_type(&fields, &TargetLayout::ilp32(), &|token| {
            value_type_layout_across(
                owner,
                Some(self.argument_world()),
                token,
                &self.references,
                &TargetLayout::ilp32(),
            )
        })
        .ok()
    }

    /// The heap layout of ONE INSTANTIATION, named by the `TypeSpec` token that spells it: the
    /// base chain's blocks as usual, then the definition's own block SUBSTITUTED, under a handle
    /// minted from the canonical name.
    ///
    /// This answers the LAYOUT alone. Whether the instantiation can be DISPATCHED on is
    /// [`Self::instantiation_dispatch`], and whether the build may proceed without either is
    /// `build::refuse_undispatchable_instantiations`.
    ///
    /// **THE VIRTUAL/INTERFACE REFUSAL USED TO LIVE HERE, AND HERE IS A FILTER RATHER THAN A
    /// GATE.** Declining the LAYOUT withheld the descriptor and let the image out with none: the
    /// bodies still lowered, the allocation still proceeded, and the dispatch went through a
    /// descriptor that was not there. MEASURED, one variable changed -- a program whose generic
    /// definition gained one `virtual` BUILT CLEANLY and then HARD FAULTED on an emulated
    /// Cortex-M0 where the same program without it answered 42. A refusal on the way to an emitter's
    /// INPUT stops a product being made; only a refusal where the build can FAIL stops the build.
    fn instantiated_reference_layout(&self, spec: Token) -> Option<ReferenceLayout> {
        let (owner, type_def, arguments) = self.instantiated_parent(spec)?;
        if type_def.is_value_type() {
            return None;
        }
        let layout_args =
            caller_resolved_arguments(&arguments, self.argument_world(), &self.references)?;
        let chain = self.cross_class_chain(owner, type_def, &layout_args);
        let mut size = 0u32;
        let mut reference_offsets = Vec::new();
        for link in &chain {
            let layout = self.link_layout(link)?;
            for offset in &layout.reference_offsets {
                reference_offsets.push(size + offset);
            }
            size = (size + layout.size).next_multiple_of(4);
        }
        let spec_type = self.closed_spec_signature(spec)?;
        let name = crate::generics::spell_sig(self.assembly, &spec_type)?;
        Some(ReferenceLayout {
            handle: crate::generics::instantiation_handle(&name),
            size,
            reference_offsets,
        })
    }

    /// The payload offset where `type_def`'s OWN field block starts: the word-aligned sum of
    /// every base block before it -- [`Self::reference_layout_of`]'s accumulation, stopped
    /// before `type_def` (the chain's LAST entry by construction).
    fn class_block_start(&self, owner: &'a Assembly<'a>, type_def: TypeDef<'a>) -> Option<u32> {
        let chain = self.cross_class_chain(owner, type_def, &[]);
        let mut start = 0u32;
        for link in &chain[..chain.len() - 1] {
            start = (start + self.link_layout(link)?.size).next_multiple_of(4);
        }
        Some(start)
    }

    /// ONE chain link's own field block, laid out under the arguments in force FOR THAT LINK.
    ///
    /// **EVERY WALK OVER A CLASS CHAIN GOES THROUGH HERE, AND THAT IS WHAT MAKES A GENERIC BASE
    /// SAFE TO FOLLOW AT ALL.** Before the chain could cross a `TypeSpec` there was nothing to
    /// decide: every link was non-generic and every caller spelled `value_type_layout` itself. A
    /// link with arguments laid out that way reads its fields as the OPEN definition declares them
    /// -- `T value` sized from `!0` -- which is a wrong block rather than a missing one, and it
    /// would land in three callers independently.
    fn link_layout(&self, link: &ChainLink<'a>) -> Option<TypeLayout> {
        if link.arguments.is_empty() {
            return self.own_block_layout(link.assembly, link.type_def);
        }
        self.instantiated_layout(link.assembly, link.type_def, &link.arguments)
    }

    /// One type's OWN field block, laid out with a nested resolver that can LEAVE `owner`.
    ///
    /// **`Assembly::value_type_layout` HANDS `layout_value_type` A SINGLE-ASSEMBLY CLOSURE, WHICH IS
    /// CORRECT FOR THE METADATA CRATE AND WRONG HERE.** That crate holds no reference list, so a
    /// field whose type is a value type declared in a REFERENCED assembly resolves to nothing there
    /// and the WHOLE class fails to lay out -- no size, no field offsets and no trace map, and every
    /// consumer of those refuses. The reference list lives in this resolver, so the closure does too.
    ///
    /// It is the ONE reader both the allocation path ([`Self::reference_layout_of`], through
    /// [`Self::link_layout`]) and the field-access path ([`Self::own_block_field_offset`]) consult,
    /// so those two cannot number one field differently -- they resolve it through separate code and
    /// each refuses on its own.
    fn own_block_layout(
        &self,
        owner: &'a Assembly<'a>,
        type_def: TypeDef<'a>,
    ) -> Option<TypeLayout> {
        let fields: Vec<SigType> = type_def
            .fields()
            .filter(|field| !field.is_static())
            .filter_map(|field| field.signature())
            .collect();
        layout_value_type(&fields, &TargetLayout::ilp32(), &|token| {
            value_type_layout_across(
                owner,
                None,
                token,
                &self.references,
                &TargetLayout::ilp32(),
            )
        })
        .ok()
    }

    /// [`Assembly::field_offset`] over [`Self::own_block_layout`]: the block-relative offset of
    /// `field` within `type_def` (declared in `owner`), so a field sitting AFTER a cross-assembly
    /// value-type field is still addressable. Position among the instance fields, exactly the
    /// numbering the metadata crate's own version uses.
    fn own_block_field_offset(
        &self,
        owner: &'a Assembly<'a>,
        type_def: TypeDef<'a>,
        field: Token,
    ) -> Option<u32> {
        let index = type_def
            .fields()
            .filter(|candidate| !candidate.is_static())
            .position(|candidate| candidate.token() == field)?;
        self.own_block_layout(owner, type_def)?
            .field_offsets
            .get(index)
            .copied()
    }
}

/// The AOT's interface-method identity tag: FNV-1a32 of the interface's full name, the method name, and
/// a byte per parameter type, with the high bit set (the shared type/exception tag space). A
/// `callvirt IFoo::Bar(args)` and every implementing type's itable entry for it derive the SAME tag, so
/// dispatch needs no shared registry.
///
/// SCOPE, corrected: this is the AOT's tag and nothing else computes it. An earlier revision of this
/// note said the interpreter computes it identically; it does not -- `lamella-cil-runtime` reproduces
/// the EXCEPTION tag (`exception.rs`) and dispatches interfaces by signature key, never by this hash.
/// What the tag IS, is cross-ASSEMBLY ABI: it is baked into emitted code and into itable entries in
/// type descriptors, so a program object and a library object built at different times must agree
/// about every byte of it. That is why its encoding cannot change once artifacts exist in the wild,
/// and the reason is narrower than the cross-tier one previously recorded.
///
/// WHAT GENERICS MUST SETTLE BEFORE THAT POINT, because it cannot be settled after. The interface's
/// identity here is its NAME, and a name carries no type arguments -- so `IList<Foo>::Add` and
/// `IList<Bar>::Add` hash IDENTICALLY, and both would dispatch to whichever itable entry the
/// descriptor happened to carry. Reserving [`element::VAR`] / [`element::GENERICINST`] /
/// [`element::MVAR`] (done) covers only the PARAMETER bytes; the missing half is a canonical spelling
/// for the instantiation in the hashed name, and it has to be CHOSEN rather than discovered by the
/// first program that declares two instantiations of one generic interface.
///
/// # The canonical instantiation spelling
///
/// **EVERY type argument contributes its FULL NAME, value and reference alike. One rule, no
/// exception.** `IList<int>` and `IList<long>` differ; so do `IList<string>` and `IList<Foo>`.
///
/// * **Arity comes free**: ECMA-335 already spells a generic type's name with its backtick-arity
///   suffix (`IList`1`), so `IFoo<T>` and `IFoo<T,U>` separate with no invented rule.
/// * **Nested instantiations recurse** (`IList<List<int>>`), so the spelling is defined
///   structurally rather than as a flat list.
///
/// **THE SHORTCUT TO REJECT, AND WHY, BECAUSE IT LOOKS RIGHT.** If reference instantiations
/// share one body, a reference argument could contribute one shared marker ([`element::CLASS`])
/// instead of its name, and `IList<string>` and `IList<Foo>` would hash alike. **That conflates two
/// identities this one value carries at once:**
///
/// * **WHICH BODY RUNS.** Reference instantiations may share one, and that is a code-model choice.
/// * **WHICH TYPE THIS IS.** [`CallResolver::cast_interface_tag`] derives an interface's identity
///   for `isinst`/`castclass` from this same hash, and `Inst::InterfaceHasTag` answers the test by
///   scanning the receiver's itable for it. **A shared marker makes `IList<string>` and
///   `IList<Foo>` the SAME TEST**, so a type implementing only `IList<Foo>` answers `true` to
///   `o is IList<string>` and the cast that follows succeeds. **Sharing a body is not sharing an
///   identity**, and that hole holds under the sharing code model too -- it does not need anyone to
///   monomorphize reference types first.
///
/// **A tag may name one body under two tags; it must never name two bodies -- or two types -- under
/// one.** [`TypeMeta::itable`] is a list of `(tag, implementation)` pairs and the emitted scan
/// compares tags, so two tags carrying the same implementation word is exactly representable. That
/// is why the by-name spelling is correct under BOTH code models and the collapsed one under at most
/// one, and its whole price is one extra 8-byte itable entry per additional instantiation per
/// implementing type, paid only where instantiations share.
///
/// **So the spelling does not depend on the code model at all**, and freezing it forecloses nothing:
/// whether reference instantiations share a body stays a backend decision, as it should be -- a
/// tunable with a measured break-even near R = 2.79 (the generics spike's value-type fit), not
/// something this tag settles.
///
/// **THE HOLE ABOVE IS A READING, NOT A MEASUREMENT, AND IT IS NOT REACHABLE TODAY** -- generics
/// are unimplemented, so no program can construct the collision and no test can currently fail on
/// it. That is precisely why it had to be settled before the freeze rather than after: the first
/// program that could exhibit it is the first to declare two instantiations of one generic
/// interface, which is not an exotic program.
///
/// **NOTE THE ASYMMETRY WITH [`sig_element_byte`], WHICH IS DELIBERATE.** A PARAMETER contributes
/// only its kind byte, so `IFoo.Bar(MyStruct)` and `IFoo.Bar(OtherStruct)` already tag alike -- a
/// known imprecision, tolerable because a parameter is only distinguishing OVERLOADS. A type
/// ARGUMENT distinguishes TYPES, which is a correctness question, so it carries more. The two
/// answer different questions and the difference is the reason, not an inconsistency.
///
/// [`element::VAR`]: lamella_metadata::signature::element::VAR
/// [`element::GENERICINST`]: lamella_metadata::signature::element::GENERICINST
/// [`element::MVAR`]: lamella_metadata::signature::element::MVAR
/// [`element::CLASS`]: lamella_metadata::signature::element::CLASS
/// `None` when a parameter names a type this assembly cannot resolve to a name -- see
/// [`fold_tag_element`], which is the only arm that can refuse. **A caller must SKIP the member
/// rather than substitute a value**, for the same reason [`decodable_params`] gives: a tag computed
/// from a fabricated parameter is a DISPATCH KEY that silently belongs to a different method.
#[must_use]
pub fn interface_method_tag(
    assembly: &Assembly<'_>,
    interface: &TypeName,
    method: &str,
    params: &[SigType],
) -> Option<u32> {
    Some(interface_method_hash(assembly, interface, method, params)? | 0x8000_0000)
}

/// A GENERIC interface method's tag at ONE instantiation of its own type parameters --
/// `ITag.Tag<int>` as distinct from `ITag.Tag<string>`.
///
/// # Why the arguments have to be in the tag at all
///
/// [`interface_method_tag`]'s own criterion decides it: *a tag may name one body under two tags; it
/// must never name two bodies -- or two types -- under one.* `Tag<int>` and `Tag<string>` are two
/// bodies of one declaration, so a tag folded from the declaration alone names both, and the itable
/// scan reaches whichever entry was written first. That is a wrong answer, not a missing one.
///
/// # It is ADDITIVE, which is what lets it land under a frozen encoding
///
/// The tag is an ABI a later build must reproduce byte for byte, unlike the vtable slot key. Nothing
/// this computes moves an existing value: a NON-GENERIC method never reaches this function, and a
/// generic one had no working dispatch at all -- the call site computes no tag for a `MethodSpec`
/// token, which is exactly why `genvgeniface` refuses. So the values that change are values nothing
/// has ever consumed.
///
/// # The separator, which is insurance rather than arithmetic
///
/// A `<` is folded before the arguments so this function's output space and
/// [`interface_method_tag`]'s are disjoint by construction. Nothing needs it today -- within one
/// interface and one method name the parameters are fixed by the declaration, so only the arguments
/// vary -- but it means a caller that reaches for the WRONG one of the two gets a tag matching
/// NOTHING rather than a tag matching another method.
///
/// # The two assemblies are not the same one and that is deliberate
///
/// `assembly` reads the interface's own signature -- the parameters are keyed by NAME so the two
/// sides of a cross-assembly dispatch agree -- while `arguments_assembly` is the module whose
/// `MethodSpec` rows the arguments were decoded from. Folding the caller's arguments against the
/// interface's tables would resolve a token in a world that never wrote it.
#[must_use]
pub fn instantiated_interface_method_tag(
    assembly: &Assembly<'_>,
    interface: &TypeName,
    method: &str,
    params: &[SigType],
    arguments_assembly: &Assembly<'_>,
    arguments: &[SigType],
) -> Option<u32> {
    let mut hash = interface_method_hash(assembly, interface, method, params)?;
    hash = fnv1a32(hash, b"<");
    for argument in arguments {
        hash = fold_tag_element(arguments_assembly, hash, argument)?;
    }
    Some(hash | 0x8000_0000)
}

/// The fold both tag spellings share, WITHOUT the high bit.
///
/// It exists so the instantiated spelling is the ordinary one plus a suffix rather than a second
/// transcription of the interface name, the dot, the method name and the parameter fold -- four
/// things that would then have two implementations, and a tag with two implementations is a
/// dispatch that works until someone corrects one of them.
fn interface_method_hash(
    assembly: &Assembly<'_>,
    interface: &TypeName,
    method: &str,
    params: &[SigType],
) -> Option<u32> {
    let mut hash = 0x811c_9dc5u32;
    if !interface.namespace.is_empty() {
        hash = fnv1a32(hash, interface.namespace.as_bytes());
        hash = fnv1a32(hash, b".");
    }
    hash = fnv1a32(hash, interface.name.as_bytes());
    hash = fnv1a32(hash, b".");
    hash = fnv1a32(hash, method.as_bytes());
    for param in params {
        hash = fold_tag_element(assembly, hash, param)?;
    }
    Some(hash)
}

/// One parameter's contribution to an interface-method tag: its element byte, plus -- for the three
/// GENERIC element types only -- the payload that byte cannot carry.
///
/// **EVERY NON-GENERIC PARAMETER FOLDS EXACTLY ONE BYTE, WHICH IS WHAT MAKES THIS ADDITIVE.** No
/// tag that exists today moves; [`tests::the_interface_tag_spelling_is_pinned_to_literal_values`]
/// is the guard, and it pins six literals that must not change.
///
/// **Why the generic cases need more than a byte.** `element::GENERICINST` is one value, so folding
/// it alone would make `IFoo.Bar(List<int>)` and `IFoo.Bar(List<string>)` the same tag -- and worse,
/// the same tag as `IFoo.Bar(HashSet<int>)`. The arguments are therefore folded too, recursively, so
/// an instantiation contributes its shape rather than the fact of being one. Likewise `!0` and `!1`
/// are different types, so the parameter NUMBER is folded after `VAR`/`MVAR`.
///
/// **AN INSTANTIATION FOLDS ITS CANONICAL SPELLING, WHICH IS WHY THIS TAKES AN `Assembly`.** The
/// element byte says only "an instantiation": folding it alone would make `IFoo.Bar(List<int>)`,
/// `IFoo.Bar(List<string>)` and `IFoo.Bar(HashSet<int>)` one tag. Folding the byte plus the
/// definition's KIND byte and its arguments' -- which is what this did before -- separates
/// `List<int>` from `List<string>` and leaves `List<int>` and `HashSet<int>` sharing one, because a
/// definition token contributes only the `CLASS` byte. **The name is the only identity that
/// survives the assembly boundary**, and it comes from [`crate::generics::spell_sig`], the project's
/// one canonical spelling, so this cannot drift from what the monomorphizer and the interpreter's
/// loader call the same type.
///
/// **EVERY AMBIGUOUS POSITION FOLDS A NAME.** Folding only the element byte at the non-generic
/// positions -- `CLASS`, `VALUETYPE`, `PTR`, `BYREF`, `SZARRAY`, `ARRAY` -- collapses two overloads
/// that differ solely in a named type, so each of those folds a name too.
///
/// **THE SHAPE THAT DECIDES IT IS AN ORDINARY BCL ONE.** `BinaryWriter` declares
/// `Write(byte[] buffer)` beside `Write(char[] chars)`, both virtual. Under the byte-only fold those
/// are one dispatch key. That it sits on a class rather than an interface is luck, not a property.
///
/// **`!n` AND `!!n` FOLD THEIR NUMBER, NOT A NAME, AND MUST.** `IList<T>::Add(!0)` is the same
/// method under every instantiation of `IList<T>`, so its parameter is genuinely the same type
/// parameter each time. The two spaces stay distinct because their element bytes differ.
///
/// **A PRIMITIVE KEEPS ITS BYTE, AND THAT IS NOT AN INCONSISTENCY.** The byte-to-name map is a
/// BIJECTION over the eighteen payload-free element types, so `0x08` and `System.Int32` separate
/// exactly the same parameters. Spelling them would carry no more information and would move every
/// tag with any parameter at all -- strictly more cost for identically zero discrimination.
///
/// **The cast hole `generics-identity-and-sharing` s2 forbids lives somewhere else and is closed
/// somewhere else.** `o is IList<string>` answering true for a type implementing only `IList<Foo>`
/// would be a collision in the INTERFACE's identity -- the `interface` argument above, whose
/// `TypeName` carries the canonical instantiation spelling ([`crate::generics`], and s6 of that
/// doc).
///
/// `None` when a token in an instantiation cannot be resolved to a name. **That is a refusal and not
/// a degradation:** folding the bare byte instead would put two instantiations under one dispatch
/// key, which is the whole defect this arm closes.
fn fold_tag_element(assembly: &Assembly<'_>, hash: u32, ty: &SigType) -> Option<u32> {
    Some(match ty {
        SigType::Var(number) => {
            let hash = fnv1a32(hash, &[sig_element_byte(ty)]);
            fnv1a32(hash, &number.to_le_bytes())
        }
        SigType::MVar(number) => {
            let hash = fnv1a32(hash, &[sig_element_byte(ty)]);
            fnv1a32(hash, &number.to_le_bytes())
        }
        SigType::GenericInst { .. }
        | SigType::Class(_)
        | SigType::ValueType(_)
        | SigType::Pointer(_)
        | SigType::ByRef(_)
        | SigType::SzArray(_)
        | SigType::Array { .. } => {
            let hash = fnv1a32(hash, &[sig_element_byte(ty)]);
            fnv1a32(hash, crate::generics::spell_sig(assembly, ty)?.as_bytes())
        }
        other => fnv1a32(hash, &[sig_element_byte(other)]),
    })
}

/// The ECMA-335 element-type byte a `SigType` is spelled with, folded into an interface-method tag
/// to distinguish overloads.
///
/// **THE TABLE MOVED TO `lamella-metadata` AND THIS IS THE ALIAS, WHICH IS THE POINT.** It maps a
/// `lamella-metadata` type onto `lamella-metadata` constants, so it never belonged in the backend;
/// keeping it here made it `pub(crate)`, which meant a SECOND consumer of the canonical spelling
/// could not reach it without either an extraction or its own copy of the table. It now sits beside
/// the decoder it must agree with -- see [`lamella_metadata::signature::element_byte`] for the
/// stability note and for why one byte is insufficient as an IDENTITY for the three generic
/// variants.
pub(crate) use lamella_metadata::signature::element_byte as sig_element_byte;

impl CallResolver for MetadataResolver<'_> {
    fn resolve(&self, operand: &Operand) -> Option<CallInfo> {
        let Operand::Token(token) = operand else {
            return None;
        };
        if let Some(info) = self.monomorphized_method_call(*token) {
            return Some(info);
        }
        let method = self.assembly.resolve_method(*token)?;
        let signature = method.signature.as_ref()?;
        if let Some(info) = self.monomorphized_call(*token, signature) {
            return Some(info);
        }
        if let Some(info) = self.monomorphized_self_call(*token, signature) {
            return Some(info);
        }
        if let Some(info) = self.instantiated_interface_call(*token, signature) {
            return Some(info);
        }
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
            MethodKind::Definition(rid) => self.own_call_target(rid)?,
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
                let declaring = method.declaring_type.as_ref()?;
                if declaring.name.is_empty() {
                    return None;
                }
                let (namespace, type_name) = (declaring.namespace, declaring.name);
                let name = method.name?;
                CallTarget::External(
                    extern_method_symbol(
                        namespace,
                        type_name,
                        name,
                        &signature.parameters,
                        &signature.return_type,
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
                if member.parent().table() == table::TYPE_SPEC {
                    let (owner, type_def, arguments) = self.instantiated_parent(member.parent())?;
                    let name = member.name()?;
                    let index = type_def
                        .fields()
                        .filter(|field| !field.is_static())
                        .position(|field| field.name() == Some(name))?;
                    let arguments =
                        caller_resolved_arguments(&arguments, self.argument_world(), &self.references)?;
                    let layout = self.instantiated_layout(owner, type_def, &arguments)?;
                    return Some(
                        self.class_block_start(owner, type_def)?
                            + *layout.field_offsets.get(index)?,
                    );
                }
                let parent = self.assembly.type_token_name(member.parent())?;
                let field_name = member.name()?;
                let (_, owner, type_def) =
                    self.find_reference_type(parent.namespace, parent.name)?;
                let field = type_def
                    .fields()
                    .find(|f| f.name() == Some(field_name))
                    .filter(|f| !f.is_static())?;
                let block = self.own_block_field_offset(owner, type_def, field.token())?;
                Some(self.class_block_start(owner, type_def)? + block)
            }
            _ => {
                let declaring = self
                    .assembly
                    .type_defs()
                    .find(|type_def| type_def.fields().any(|field| field.token() == *token))?;
                let block = match self.own_block_field_offset(self.assembly, declaring, *token) {
                    Some(block) => block,
                    None if !self.layout_arguments.is_empty() => {
                        let index = declaring
                            .fields()
                            .filter(|field| !field.is_static())
                            .position(|field| field.token() == *token)?;
                        let layout = self.instantiated_layout(
                            self.assembly,
                            declaring,
                            &self.layout_arguments,
                        )?;
                        *layout.field_offsets.get(index)?
                    }
                    None => return None,
                };
                Some(self.class_block_start(self.assembly, declaring)? + block)
            }
        }
    }

    fn field_type(&self, operand: &Operand) -> Option<MirType> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let signature = match token.table() {
            table::MEMBER_REF => {
                let member = self.assembly.member_ref(token.row())?;
                let declared = member.field_type()?;
                if member.parent().table() == table::TYPE_SPEC {
                    let (_, _, arguments) = self.instantiated_parent(member.parent())?;
                    let arguments = caller_resolved_arguments(
                        &arguments,
                        self.argument_world(),
                        &self.references,
                    )?;
                    crate::generics::substitute_sig(&declared, &arguments)?
                } else {
                    declared
                }
            }
            _ => self
                .apply_instantiation(&self.assembly.field_signature(*token)?)?,
        };
        mir_type_across(
            &signature,
            self.assembly,
            self.argument_assembly,
            &self.references,
            &TargetLayout::ilp32(),
        )
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

    fn value_type_cell(&self, operand: &Operand) -> Option<(u32, lamella_ir::RefWords)> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let layout = self
            .assembly
            .value_type_layout(*token, &TargetLayout::ilp32())
            .ok()?;
        Some((layout.size, ref_words_of(&layout.reference_offsets)?))
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
            refs: ref_words_of(&layout.reference_offsets)?,
        })
    }

    fn newobj_reference_layout(&self, operand: &Operand) -> Option<ReferenceLayout> {
        let Operand::Token(token) = operand else {
            return None;
        };
        if token.table() == table::MEMBER_REF
            && let Some(member) = self.assembly.member_ref(token.row())
            && member.parent().table() == table::TYPE_SPEC
        {
            return self.instantiated_reference_layout(member.parent());
        }
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

    fn delegate_invoke_args(&self, operand: &Operand) -> Option<(usize, Option<MirType>)> {
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
        let result_type = if sig.return_type == SigType::Void {
            None
        } else {
            Some(
                mir_type(&sig.return_type, self.assembly, &TargetLayout::ilp32())
                    .unwrap_or(MirType::I32),
            )
        };
        Some((sig.parameters.len(), result_type))
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
        if token.table() == table::TYPE_SPEC {
            if let Some(element) = self.substituted_array_element(*token) {
                return Some(element);
            }
        }
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
                .map(|(_, slot, _)| {
                    let owner = self
                        .reference_owner
                        .as_ref()
                        .map_or(StaticOwner::Own, |o| StaticOwner::Reference(o.ordinal));
                    (owner, slot * 4)
                }),
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

    fn type_init_thunk(
        &self,
        operand: &Operand,
        trigger: crate::cil::InitTrigger,
    ) -> Option<crate::cil::TypeInitThunk> {
        use crate::cil::{InitTrigger, TypeInitThunk};
        let Operand::Token(token) = operand else {
            return None;
        };
        let named = match trigger {
            InitTrigger::StaticField => match token.table() {
                table::FIELD => NamedType::Own(
                    self.assembly
                        .type_defs()
                        .find(|td| td.fields().any(|f| f.token().row() == token.row()))?
                        .token()
                        .row(),
                ),
                table::MEMBER_REF => {
                    let member = self.assembly.member_ref(token.row())?;
                    if !member.is_field() {
                        return None;
                    }
                    let parent = self.assembly.type_token_name(member.parent())?;
                    let (_, owner, type_def) =
                        self.find_reference_type(parent.namespace, parent.name)?;
                    NamedType::Reference(owner, type_def)
                }
                _ => return None,
            },
            InitTrigger::Method => {
                let resolved = self.assembly.resolve_method(*token)?;
                let is_ctor = resolved.name == Some(".ctor");
                let is_static = if token.table() == table::METHOD_DEF {
                    self.assembly.method(token.row()).is_some_and(|m| m.is_static())
                } else {
                    resolved.signature.as_ref().is_some_and(|sig| !sig.has_this)
                };
                if !is_ctor && !is_static {
                    return None;
                }
                let declaring = resolved.declaring_type?;
                match self
                    .assembly
                    .type_defs()
                    .find(|td| td.name().is_some_and(|n| n == declaring))
                {
                    Some(type_def) => NamedType::Own(type_def.token().row()),
                    None => {
                        let (_, owner, type_def) =
                            self.find_reference_type(declaring.namespace, declaring.name)?;
                        NamedType::Reference(owner, type_def)
                    }
                }
            }
            InitTrigger::ValueTypeCall => match token.table() {
                table::TYPE_DEF => NamedType::Own(self.assembly.type_def(token.row())?.token().row()),
                table::TYPE_REF => {
                    let name = self.assembly.type_token_name(*token)?;
                    let (_, owner, type_def) = self.find_reference_type(name.namespace, name.name)?;
                    NamedType::Reference(owner, type_def)
                }
                _ => return None,
            },
        };
        match named {
            NamedType::Own(row) if self.reference_owner.is_some() => {
                let type_def = self.assembly.type_def(row)?;
                cross_assembly_type_init(self.assembly, &type_def)
                    .map(|(_, symbol)| TypeInitThunk::Extern(symbol))
            }
            NamedType::Own(row) => self
                .type_init_thunks
                .iter()
                .find(|(type_row, _)| *type_row == row)
                .map(|(_, index)| TypeInitThunk::Local(*index)),
            NamedType::Reference(owner, type_def) => cross_assembly_type_init(owner, &type_def)
                .map(|(_, symbol)| TypeInitThunk::Extern(symbol)),
        }
    }

    fn exception_tag(&self, operand: &Operand) -> Option<u32> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let type_token = self.type_token_of(*token)?;
        if let Some(tag) = self.instantiation_exception_tag(type_token) {
            return Some(tag);
        }
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
        if let Some(tag) = self.instantiation_exception_tag(catch_token) {
            return alloc::vec![tag];
        }
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
        if target.table() == table::TYPE_SPEC {
            let (interface_assembly, interface, _) = self.instantiated_parent(target)?;
            if !interface.is_interface() {
                return None;
            }
            let closed = self.closed_spec_signature(target)?;
            let identity = InterfaceIdentity::instantiated(self.assembly, &closed)?;
            let spelled = identity.type_name();
            return interface.methods().find_map(|method| {
                let signature = method.signature()?;
                interface_method_tag(
                    interface_assembly,
                    &spelled,
                    method.name()?,
                    &signature.parameters,
                )
            });
        }
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
        interface.methods().find_map(|method| {
            let signature = method.signature()?;
            interface_method_tag(
                interface_assembly,
                &name,
                method.name()?,
                &signature.parameters,
            )
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

    fn unbox_accepted_handles(&self, operand: &Operand) -> Vec<TypeHandle> {
        let Operand::Token(token) = operand else {
            return Vec::new();
        };
        let own = self.qualified_type_handle(*token);
        let Some(form) = unbox_normal_form(self.assembly, *token, self.references()) else {
            return alloc::vec![own];
        };
        let mut handles = alloc::vec![own];
        for candidate in &self.box_target_tokens {
            if *candidate != *token
                && unbox_normal_form(self.assembly, *candidate, self.references()) == Some(form)
            {
                handles.push(self.qualified_type_handle(*candidate));
            }
        }
        handles.sort_unstable_by_key(|h| h.0);
        handles.dedup_by_key(|h| h.0);
        handles
    }

    fn box_is_noop(&self, operand: &Operand) -> bool {
        let Operand::Token(token) = operand else {
            return false;
        };
        if token.table() != table::TYPE_SPEC {
            return false;
        }
        let Some(signature) = self.assembly.type_spec_signature(*token) else {
            return false;
        };
        let Some(closed) = self.apply_instantiation(&signature) else {
            return false;
        };
        is_reference_signature(&closed)
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
        if let Some(closed) = self.closed_operand_sig(*token) {
            return mir_type_across(
                &closed,
                self.assembly,
                self.argument_assembly,
                self.references(),
                &TargetLayout::ilp32(),
            );
        }
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
            refs: ref_words_of(&layout.reference_offsets)?,
        })
    }

    fn type_operand_mir(&self, operand: &Operand) -> Option<MirType> {
        let Operand::Token(token) = operand else {
            return None;
        };
        if let Some(closed) = self.closed_operand_sig(*token) {
            return mir_type_across(
                &closed,
                self.assembly,
                self.argument_assembly,
                self.references(),
                &TargetLayout::ilp32(),
            );
        }
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
                refs: ref_words_of(&layout.reference_offsets)?,
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
                let signature = method.signature.as_ref()?;
                let key = param_key(
                    self.assembly,
                    signature.generic_param_count,
                    &signature.parameters,
                );
                if let Some(member) = self.assembly.member_ref(token.row())
                    && member.parent().table() == table::TYPE_SPEC
                {
                    let (owner, type_def, _) = self.instantiated_parent(member.parent())?;
                    if type_def.is_interface() {
                        return None;
                    }
                    let slots = if core::ptr::eq(owner, self.assembly) {
                        self.vtable_methods(type_def)
                    } else {
                        reference_vtable_slots(&self.references, owner, type_def)
                    };
                    return slots
                        .iter()
                        .position(|slot| slot.name == method.name && slot.key == key);
                }
                let declaring = method.declaring_type.as_ref()?;
                let (_, reference, ref_td) =
                    self.find_reference_type(declaring.namespace, declaring.name)?;
                if ref_td.is_interface() {
                    return None;
                }
                reference_vtable_slots(&self.references, reference, ref_td)
                    .iter()
                    .position(|slot| slot.name == method.name && slot.key == key)
            }
            table::METHOD_SPEC => match self.virtual_generic_dispatch(*token)? {
                GenericDispatch::Slot(slot) => Some(slot),
                GenericDispatch::Tag(_) => None,
            },
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
                let params = decodable_params(&method)?;
                let iface_name = self.assembly.type_token_name(type_token)?;
                interface_method_tag(self.assembly, &iface_name, name, &params)
            }
            table::MEMBER_REF => {
                let member = self.assembly.member_ref(token.row())?;
                if member.parent().table() == table::TYPE_SPEC {
                    let (_, type_def, _) = self.instantiated_parent(member.parent())?;
                    if !type_def.is_interface() {
                        return None;
                    }
                    let closed = self.closed_spec_signature(member.parent())?;
                    let identity = InterfaceIdentity::instantiated(self.assembly, &closed)?;
                    let method = self.assembly.resolve_method(*token)?;
                    let signature = method.signature.as_ref()?;
                    return interface_method_tag(
                        self.assembly,
                        &identity.type_name(),
                        method.name?,
                        &signature.parameters,
                    );
                }
                let method = self.assembly.resolve_method(*token)?;
                let declaring = method.declaring_type?;
                let (_, _, ref_td) =
                    self.find_reference_type(declaring.namespace, declaring.name)?;
                if !ref_td.is_interface() {
                    return None;
                }
                let signature = method.signature.as_ref()?;
                interface_method_tag(
                    self.assembly,
                    &declaring,
                    method.name?,
                    &signature.parameters,
                )
            }
            table::METHOD_SPEC => match self.virtual_generic_dispatch(*token)? {
                GenericDispatch::Tag(tag) => Some(tag),
                GenericDispatch::Slot(_) => None,
            },
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
        let key = param_key(
            self.assembly,
            signature.generic_param_count,
            &signature.parameters,
        );
        let own = type_def.methods().find(|m| {
            m.is_virtual()
                && m.name() == Some(name)
                && decodable_signature(m).is_some_and(|sig| {
                    param_key(self.assembly, sig.generic_param_count, &sig.parameters) == key
                })
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
            target: self.own_call_target(own.rid())?,
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
///
/// The ONE-WORLD entry point: every token in `sig` is read against `assembly`, which is what every
/// ordinary body wants. [`mir_type_across`] is the same function for a body whose type ARGUMENTS
/// were written somewhere else, and this delegates to it rather than restating the mapping, so the
/// two cannot answer differently about one signature.
fn mir_type<'x>(sig: &SigType, assembly: &'x Assembly<'x>, target: &TargetLayout) -> Option<MirType> {
    mir_type_across(sig, assembly, None, &[], target)
}

/// [`mir_type`] for a signature whose ARGUMENT-DERIVED tokens belong to another assembly -- the
/// twin of `generics::spell_sig_across`, and the same rule about when it differs: with
/// `argument_world` of `None` this is `mir_type` for every signature, because nothing is marked.
///
/// `argument_world` is the assembly the type arguments were written in, and it is `Some` only for a
/// resolver [`rebased_on_reference`](MetadataResolver::rebased_on_reference) -- where `assembly` has
/// become the definition's OWNER while the arguments stayed the CALLER's.
///
/// **THE MARK RIDES THE HANDLE ONLY WHILE THE TWO WORLDS DIFFER.** A value-type slot's handle IS its
/// token, and `build::rebase_identities` respells every such handle from the owner's numbering into
/// the caller's -- which would take a row that is ALREADY the caller's and name the owner's row of
/// that number, a real unrelated type. Leaving the mark on is what lets that pass tell the two
/// apart. Where the worlds are the same there is nothing to tell apart and no rebasing to survive,
/// so the handle is the plain token and the image is the one it always was.
fn mir_type_across<'x>(
    sig: &SigType,
    assembly: &'x Assembly<'x>,
    argument_world: Option<&'x Assembly<'x>>,
    references: &[&'x Assembly<'x>],
    target: &TargetLayout,
) -> Option<MirType> {
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
        SigType::GenericInst { definition, .. } if matches!(**definition, SigType::Class(_)) => {
            MirType::ObjectRef
        }
        SigType::ValueType(token) => match enum_underlying(assembly, *token, references, target) {
            Some(underlying) => underlying,
            None => {
                let layout =
                    value_type_layout_across(assembly, argument_world, *token, references, target)?;
                MirType::ValueType {
                    handle: TypeHandle(marked_handle_token(*token, argument_world).0),
                    size: layout.size,
                    refs: ref_words_of(&layout.reference_offsets)?,
                }
            }
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

/// The types this assembly declares that demand PRECISE initializer timing, as
/// `(TypeDef row, .cctor MethodDef rid, region slot of the "already ran" flag)`, in metadata order.
///
/// A type qualifies when it declares a `.cctor` and does NOT carry `beforefieldinit` (ECMA-335
/// II.23.1.15, semantics I.8.9.5). Both halves matter and the second is the whole saving: a relaxed
/// type's initializer may run at any time before first field access, so running it from the startup
/// chain is conformant and its access sites cost nothing.
///
/// **RELAXED IS NOT OPTIONAL.** It licenses running the initializer EARLY, never skipping it -- a
/// relaxed type whose initializer never runs answers from zeroed storage, which is what
/// `static-init-corlib` scores. So this function names the types that need a TRIGGER, not the types
/// that need initializing.
///
/// The flag slots are numbered from the end of [`static_field_slots`] so the two cannot overlap, and
/// they are assigned HERE rather than by the caller because the region size and the offsets written
/// into it must come from one walk -- writing that condition twice is how this backend's two
/// `mir_type` twins drifted.
pub(crate) fn precise_init_types(assembly: &Assembly) -> Vec<(u32, u32, u32)> {
    let mut next = static_field_slots(assembly)
        .iter()
        .map(|(_, slot, words)| slot + words)
        .max()
        .unwrap_or(1);
    let mut types = Vec::new();
    for type_def in assembly.type_defs() {
        let Some(cctor) = precise_init_cctor(&type_def) else {
            continue;
        };
        types.push((type_def.token().row(), cctor, next));
        next += 1;
    }
    types
}

/// Which assembly declares the type a trigger site names: this one (by `TypeDef` row) or a
/// reference (by owner + its `TypeDef`). The two are kept apart because the ANSWER differs in kind
/// -- a function index here, an exported symbol there -- and collapsing them to a row would lose
/// which assembly's row it is, which is how one type would end up with two flag words.
enum NamedType<'a> {
    Own(u32),
    Reference(&'a Assembly<'a>, TypeDef<'a>),
}

/// THE predicate: the `MethodDef` rid of a type's initializer when that type demands PRECISE timing
/// -- it declares a `.cctor` and does not carry `beforefieldinit` (ECMA-335 II.23.1.15, semantics
/// I.8.9.5). `None` for a relaxed type and for one with no initializer at all, which are different
/// facts with the same consequence here: no trigger is owed.
///
/// **ONE PREDICATE BECAUSE IT IS ASKED FROM TWO DIRECTIONS.** [`precise_init_types`] asks it of
/// every type in the assembly being built, to number the flag words and emit the thunks;
/// [`cross_assembly_type_init`] asks it of ONE type in a REFERENCED assembly, to decide whether an
/// access site here must call into that assembly's object. Written out twice, the two would have
/// to agree -- and a disagreement is not a lost optimization, it is an initializer whose `.cctor`
/// was dropped from the startup chain by one reading and given no trigger by the other.
pub(crate) fn precise_init_cctor(type_def: &TypeDef) -> Option<u32> {
    if type_def.is_before_field_init() {
        return None;
    }
    type_def
        .methods()
        .find(|m| m.is_static() && m.name() == Some(".cctor"))
        .map(|m| m.rid())
}

/// What a site in ANOTHER assembly must do when it touches `type_def`, declared by `owner`: the
/// initializer's rid in the owner, and the symbol the owner's object exports its thunk under.
/// `None` when the type needs no trigger, and when the thunk cannot be NAMED (below).
///
/// **BOTH SIDES OF THE LINK ASK THIS ONE FUNCTION, AND THEY MUST.** A program's startup drops a
/// referenced `.cctor` from its chain exactly when this answers `Some`, and a trigger site calls the
/// symbol exactly when this answers `Some`. Two spellings of the rule would have to agree, and the
/// disagreement in the dropping direction is silent: an initializer with no caller, which is a wrong
/// answer rather than a later start (`static-init-corlib` scores that shape as 2 instead of 42).
///
/// A reference built from a bare metadata image ([`Assembly::from_image`]) has no file bytes, so
/// there is no content hash and no `L<hash>.` symbol family to call into. That answers `None`, which
/// keeps the type EAGER on both sides at once -- the chain keeps its `.cctor` and no site calls a
/// symbol nobody emitted.
pub(crate) fn cross_assembly_type_init(
    owner: &Assembly,
    type_def: &TypeDef,
) -> Option<(u32, String)> {
    let cctor = precise_init_cctor(type_def)?;
    Some((cctor, type_init_thunk_symbol(owner, type_def.token().row())?))
}

/// The symbol one type's initialization thunk is exported under by the assembly that DECLARES it:
/// `L<hash>.init<type_row>`.
///
/// **NAMED FROM METADATA, NEVER FROM AN INDEX.** `<hash>` is the fnv1a32 of the declaring
/// assembly's CIL bytes -- the same hash its internal `L<hash>.f<rid>` symbols, its descriptors and
/// its statics region already carry -- and `<type_row>` is the `TypeDef` row. Both sides of the link
/// hold the metadata, so neither computes an index: a program re-deriving the library's thunk BASE
/// (`max_rid + 1 + plan.len()`) would be two derivations that must agree, which is this backend's
/// recurring bug class and the reason `kept_regardless` and `static_field_slots` are each one
/// predicate.
///
/// `None` for an assembly with no file bytes to hash -- see [`cross_assembly_type_init`].
pub(crate) fn type_init_thunk_symbol(assembly: &Assembly, type_row: u32) -> Option<String> {
    let bytes = assembly.file()?;
    Some(alloc::format!(
        "L{:08x}.init{type_row}",
        lamella_metadata::fnv1a32(0x811c_9dc5, bytes)
    ))
}

/// The word count one assembly's static region spans, INCLUDING the reserved word 0 -- the one
/// derivation of its size, so a region and the offsets written into it come from the same walk.
/// Gated with its only caller (`build::assembly_statics`): the WASM path places its statics at a
/// fixed base and emits no region record, so a wasm-only build would carry this unused.
///
/// It spans the type-initializer flags as well as the fields. A region sized to the fields alone
/// would place every flag past its end, where the next region's first words are -- so a type
/// initializing itself would write into a neighbor's statics, and the two assemblies would disagree
/// silently rather than fail to link.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
pub(crate) fn static_region_words(assembly: &Assembly) -> u32 {
    let fields = static_field_slots(assembly)
        .iter()
        .map(|(_, slot, words)| slot + words)
        .max()
        .unwrap_or(1);
    precise_init_types(assembly)
        .last()
        .map_or(fields, |(_, _, slot)| slot + 1)
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

/// A stable cross-assembly symbol for a managed method -- its dotted full name, an encoding of each
/// parameter type, and an encoding of the RETURN type, so every overload gets a distinct symbol. A
/// primitive is one char; a `Class`/`ValueType` contributes its FULL TYPE NAME (`O<name>;` /
/// `V<name>;`), so overloads differing only by a user-defined parameter type stay distinct; an
/// array/byref/pointer is a marker plus its element's encoding. `type_full_name` resolves a type token
/// to its dotted name (`None` -> "?", so the symbol is still stable if a token cannot be resolved). A
/// cross-assembly extern call and the defining library object mangle identically, so the own linker
/// pairs them: "System.Math.Max.ii.i" (int,int -> int) vs ".ll.l" (long,long -> long) vs
/// "System.DateTime.op_Subtraction.VSystem.DateTime;VSystem.TimeSpan;.VSystem.TimeSpan;".
///
/// **The return type is part of the CLI signature (II.23.2.1), so it has to be part of the symbol.**
/// A conversion operator is where that stops being theoretical: II.10.3.3 lets `op_Implicit` and
/// `op_Explicit` overload on return type alone, and `System.Device.Gpio.PinValue` really declares
/// three -- to `byte`, to `int`, to `bool` -- which is the dotnet/iot shape, not a Lamella invention.
/// Encoding only the parameters collapsed all three onto one name, and the duplicate-name demotion in
/// `library_symbol_names` then withdrew every one of them, so a driver writing `(bool)value` across an
/// assembly boundary failed to link at all.
///
/// The return encoding is separated from the parameters by `.`, which no type code can produce (a
/// primitive is a single non-`.` char and a named type is `;`-terminated), so the mangling stays
/// injective.
pub fn extern_method_symbol(
    namespace: &str,
    type_name: &str,
    method: &str,
    params: &[SigType],
    return_type: &SigType,
    type_full_name: &dyn Fn(Token) -> Option<String>,
) -> String {
    let mut codes = String::new();
    for p in params {
        encode_type(p, type_full_name, &mut codes);
    }
    let mut ret = String::new();
    encode_type(return_type, type_full_name, &mut ret);
    if namespace.is_empty() {
        alloc::format!("{type_name}.{method}.{codes}.{ret}")
    } else {
        alloc::format!("{namespace}.{type_name}.{method}.{codes}.{ret}")
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
        SigType::Void => out.push('v'),
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
    let (owner, underlying) = enum_underlying_sig(assembly, token, references)?;
    mir_type(&underlying, owner, target)
}

/// [`enum_underlying`] stopping one step earlier: the enum's underlying SIGNATURE and the assembly
/// that declares it, before anything turns it into a `MirType`.
///
/// **AN EXTRACTION RATHER THAN A SECOND READER.** Two callers need the same three facts -- resolve
/// the token, check the base is `System.Enum`, take the first instance field -- and one of them wants
/// the answer as a signature. Written out twice, the enum test would be a rule with two
/// implementations, which is this lane's recurring defect; asked once, the second cannot drift.
fn enum_underlying_sig<'x>(
    assembly: &'x Assembly<'x>,
    token: Token,
    references: &[&'x Assembly<'x>],
) -> Option<(&'x Assembly<'x>, SigType)> {
    let (owner, type_def) = resolve_value_type_def(assembly, token, references)?;
    let base = owner.type_token_name(type_def.extends())?;
    if base.namespace != "System" || base.name != "Enum" {
        return None;
    }
    let underlying = type_def
        .fields()
        .find(|field| !field.is_static())?
        .signature()?;
    Some((owner, underlying))
}

/// The bit an ARGUMENT-DERIVED value-type token wears while it travels through a body that is read
/// against another assembly's tables. It is set at substitution, read by each layout site, and gone
/// before anything is emitted: no image byte carries it.
///
/// **AN ENUM CAN BE ERASED AND A STRUCT CANNOT, WHICH IS THE WHOLE REASON THIS EXISTS.**
/// [`caller_resolved_argument`] re-expresses an enum argument as its underlying primitive, so after
/// that substitution nothing downstream needs to know which assembly the argument came from. A
/// struct has no such form -- its size, its field offsets and its trace map only a row can supply,
/// and the row number is the CALLER's while the tables are the OWNER's. The decision has to be
/// CARRIED past substitution, and by the time it lands [`layout_value_type`]'s resolver is a bare
/// `Fn(Token) -> Option<TypeLayout>` with no field context and so no access to the `Var(n)`
/// provenance the deciding site held.
///
/// **IT IS SET IN THE TABLE BYTE, SO THE TOKEN KEEPS ITS OWN TABLE AND ITS OWN ROW.** ECMA-335's
/// table tags run `0x00..=0x2B`, plus `0x70` for the user-string heap, so bit 7 of that byte is not
/// a tag and cannot collide with one -- the same collision-freedom the argument-slot handle encoding
/// rests on. A marked token therefore still says whether it is a `TypeDef` or a `TypeRef`, which an
/// argument INDEX would have discarded and a second list would have had to carry back.
///
/// **A READER WITH NO ARM REFUSES RATHER THAN MIS-READING, AND THAT IS THE POINT.**
/// [`resolve_value_type_def`] and `Assembly::value_type_layout` both match on the table tag and
/// answer nothing for a tag that is not theirs, so a marker reaching an unconverted reader produces
/// a refusal -- never the plausible wrong number this whole family exists to prevent.
///
/// **SETTING IT TWICE IS SETTING IT ONCE.** A marked list can be substituted into an open `TypeSpec`
/// and re-resolved; an idempotent mark makes that harmless, where an index would have become an
/// index of an index.
pub(crate) const ARGUMENT_WORLD_BIT: u32 = 0x8000_0000;

/// `token`, marked as one to be read in the world its ARGUMENT was written in.
fn in_argument_world(token: Token) -> Token {
    Token(token.0 | ARGUMENT_WORLD_BIT)
}

/// The token a marker carries, or `None` when `token` is an ordinary one.
pub(crate) fn argument_world_token(token: Token) -> Option<Token> {
    (token.0 & ARGUMENT_WORLD_BIT != 0).then(|| Token(token.0 & !ARGUMENT_WORLD_BIT))
}

/// [`argument_world_token`] over a type HANDLE, whose value for a value type IS its token.
///
/// The mark therefore rides one encoding into both spaces, which is what lets `rebase_identities`
/// tell a slot naming the CALLER's own struct from one naming the owner's without a second list.
pub(crate) fn argument_world_handle(handle: TypeHandle) -> Option<TypeHandle> {
    argument_world_token(Token(handle.0)).map(|token| TypeHandle(token.0))
}

/// The token a value-type SLOT should carry as its handle: the mark is kept while the arguments
/// were written somewhere other than where the body is read, and dropped when they were not.
///
/// The mark exists to survive as far as `build::rebase_identities`, which is the pass that would
/// otherwise respell a caller's own row as the owner's. A body that is not rebased never meets that
/// pass, so a mark left on one would reach the image -- a handle no descriptor carries.
pub(crate) fn marked_handle_token(token: Token, argument_world: Option<&Assembly>) -> Token {
    match (argument_world_token(token), argument_world) {
        (Some(argument), None) => argument,
        _ => token,
    }
}

/// [`lamella_ir::array_handle`] over an element handle that may carry [`ARGUMENT_WORLD_BIT`].
///
/// **THE LIFT KEYS ON THE TABLE BYTE AND A MARKED HANDLE'S TOP BYTE IS THE MARK**, so handing one
/// straight to `array_handle` takes its *"already an array identity, or names no class descriptor for
/// its array to collide with"* fall-through and returns the ELEMENT unchanged. The array and its
/// element then share one handle -- the exact collision that function exists to remove, arriving
/// through the repair for a different one. Unmark, lift, re-mark: `build::rebase_identities` drops
/// the mark afterwards, so the array reaches the image as the same identity the caller's own
/// `new T[n]` would mint for it.
///
/// Both marking arms of [`MetadataResolver::substituted_array_element`] consult this rather than
/// spelling the unmark-lift-remark at each, because the two arms differ in outcome only by whether a
/// CLASS descriptor already occupies the colliding handle -- so one of them fails loudly and the
/// other keeps its descriptor under the element's identity, which is a defect with no symptom.
fn argument_world_array_handle(element: TypeHandle) -> TypeHandle {
    match argument_world_handle(element) {
        Some(unmarked) => TypeHandle(lamella_ir::array_handle(unmarked).0 | ARGUMENT_WORLD_BIT),
        None => lamella_ir::array_handle(element),
    }
}

/// The layout of a value type named by a token, read in the world that token belongs to.
///
/// **ONE FUNCTION, BECAUSE THE MARK IS USELESS AT A READER THAT DOES NOT ASK ABOUT IT.** The three
/// places a substituted field type reaches [`layout_value_type`] each held their own
/// `owner.value_type_layout(..)` closure, and a rule spelled three times gains a case in none of
/// them. `argument_world` is `None` for every ordinary reader -- for which this is
/// `Assembly::value_type_layout` and nothing else -- and `Some` only where a body is read against
/// its owner while its arguments belong to the caller.
///
/// A marked token is resolved through [`resolve_value_type_def`], so a struct argument DECLARED IN A
/// THIRD ASSEMBLY resolves by name exactly as [`enum_underlying`] already makes an enum one.
pub(crate) fn value_type_layout_across<'x>(
    owner: &'x Assembly<'x>,
    argument_world: Option<&'x Assembly<'x>>,
    token: Token,
    references: &[&'x Assembly<'x>],
    target: &TargetLayout,
) -> Option<TypeLayout> {
    let Some(argument) = argument_world_token(token) else {
        let (declaring, type_def) = resolve_value_type_def(owner, token, references)?;
        return declaring.value_type_layout(type_def.token(), target).ok();
    };
    let (declaring, type_def) =
        resolve_value_type_def(argument_world.unwrap_or(owner), argument, references)?;
    declaring.value_type_layout(type_def.token(), target).ok()
}

/// A type ARGUMENT re-expressed so that reading it no longer depends on WHICH assembly reads it --
/// the mechanical half of *an argument resolves in the CALLER's world*.
///
/// **THE ONE SHAPE WHOSE MEANING IS A ROW LOOKUP IS A `ValueType` TOKEN**, and this resolves it
/// against the assembly that spelled the instantiation. An ENUM is its underlying integer
/// everywhere, so re-spelling it as that primitive makes it carry no assembly at all, and every
/// downstream layout reader is then correct with no provenance to track. A real STRUCT has no such
/// form -- its size, field offsets and trace map only a row can supply -- so its token is kept and
/// MARKED with [`ARGUMENT_WORLD_BIT`], which carries "read this row in the caller's tables" across
/// the substitution that would otherwise erase where it came from. Everything else is already
/// assembly-independent FOR LAYOUT: a primitive is itself, a reference is four bytes and one traced
/// word whatever it names.
///
/// **`None` IS RESERVED FOR AN ARGUMENT THAT NAMES NO VALUE TYPE AT ALL**, which its caller turns
/// into [`crate::build::MonoGap::CrossAssemblyValueTypeArgument`]. A mark this tier cannot resolve
/// is not a guess -- the reader it reaches refuses -- but an argument whose row resolves to nothing
/// in the world that wrote it is refused HERE, where the argument can still be named in the message.
///
/// **THIS IS THE LAYOUT READING AND NEVER THE SPELLING ONE.** The same argument list also
/// produces a canonical spelling, and an enum re-spelled as its underlying integer would put
/// `` Box`1[MyEnum] `` and `` Box`1[System.Int32] `` under ONE tag -- two types, one identity, which
/// is a hazard a cast cannot recover from, since a type test compares exactly that identity. Never
/// hand this to a speller; that is what [`MetadataResolver::type_arguments`] stays un-erased for.
pub(crate) fn caller_resolved_argument<'x>(
    argument: &SigType,
    caller: &'x Assembly<'x>,
    references: &[&'x Assembly<'x>],
) -> Option<SigType> {
    match argument {
        SigType::ValueType(token) if argument_world_token(*token).is_some() => {
            Some(argument.clone())
        }
        SigType::ValueType(token) => match enum_underlying_sig(caller, *token, references) {
            Some((_, underlying)) => Some(underlying),
            None => resolve_value_type_def(caller, *token, references)
                .map(|_| SigType::ValueType(in_argument_world(*token))),
        },
        other => Some(other.clone()),
    }
}

/// [`caller_resolved_argument`] over a whole list, or `None` if any argument has no
/// assembly-independent form.
pub(crate) fn caller_resolved_arguments<'x>(
    arguments: &[SigType],
    caller: &'x Assembly<'x>,
    references: &[&'x Assembly<'x>],
) -> Option<Vec<SigType>> {
    arguments
        .iter()
        .map(|argument| caller_resolved_argument(argument, caller, references))
        .collect()
}

/// The ECMA-335 element byte a value type NORMALIZES to for UNBOX compatibility -- a primitive is
/// itself, an enum is its underlying primitive, and anything else (a real struct) is `None`, meaning
/// only its own exact identity will do.
///
/// This is III.4.32/III.4.33's rule, and it is deliberately the SAME normalization the interpreter
/// applies (`interp.rs`'s `ElemRule::Unbox`: *"the exact type, with an enum standing for its exact
/// underlying primitive"*). The two tiers have to agree about which unbox throws, so the rule is
/// taken from the tier that already implements it rather than re-derived here.
///
/// **THE ELEMENT BYTE IS THE KEY BECAUSE THE COARSER ONES ARE WRONG HERE.** `MirType` collapses
/// `Int32` and `UInt32` to `I32`, and [`primitive_element_kind`] collapses them to one code because
/// it answers a question about WIDTH. Unbox asks about TYPE IDENTITY: an `int` box does NOT unbox as
/// `uint`. II.23.1.16 gives `I4` and `U4` distinct bytes, so it separates exactly the pairs
/// (`int`/`uint`, `byte`/`bool`, `char`/`ushort`) this rule must keep apart.
pub(crate) fn unbox_normal_form<'x>(
    assembly: &'x Assembly<'x>,
    token: Token,
    references: &[&'x Assembly<'x>],
) -> Option<u8> {
    if let Some(name) = assembly.type_token_name(token) {
        if let Some(sig) = primitive_sig_type(name.namespace, name.name) {
            return Some(sig_element_byte(&sig));
        }
    }
    let (owner, type_def) = resolve_value_type_def(assembly, token, references)?;
    let base = owner.type_token_name(type_def.extends())?;
    if base.namespace != "System" || base.name != "Enum" {
        return None;
    }
    let underlying = type_def
        .fields()
        .find(|field| !field.is_static())?
        .signature()?;
    if matches!(
        underlying,
        SigType::Var(_) | SigType::MVar(_) | SigType::GenericInst { .. }
    ) {
        return None;
    }
    Some(sig_element_byte(&underlying))
}

/// The `SigType` a `System` primitive's NAME denotes, so a boxed primitive can be normalized by the
/// same element-byte encoding a signature parameter uses. `None` for anything that is not one of
/// them (including `System.Enum` itself, which is abstract and never boxed as such).
fn primitive_sig_type(namespace: &str, name: &str) -> Option<SigType> {
    if namespace != "System" {
        return None;
    }
    Some(match name {
        "Boolean" => SigType::Boolean,
        "Char" => SigType::Char,
        "SByte" => SigType::I1,
        "Byte" => SigType::U1,
        "Int16" => SigType::I2,
        "UInt16" => SigType::U2,
        "Int32" => SigType::I4,
        "UInt32" => SigType::U4,
        "Int64" => SigType::I8,
        "UInt64" => SigType::U8,
        "Single" => SigType::R4,
        "Double" => SigType::R8,
        "IntPtr" => SigType::IntPtr,
        "UIntPtr" => SigType::UIntPtr,
        _ => return None,
    })
}

/// The `System` type a closed type ARGUMENT names, for the arguments that name no metadata row: the
/// primitives [`primitive_sig_type`] knows, plus `String`. `None` for everything else, which is the
/// answer that keeps a caller on its existing path rather than guessing at a token's world.
///
/// **THE INVERSE OF [`primitive_sig_type`], AND PINNED TO IT BY TEST RATHER THAN BY READING.** Two
/// spellings of one table is how a case gets added to one and not the other; the test walks every
/// name the forward table admits and asserts the round trip, so a primitive added there fails here
/// until it is added here too. `String` is deliberately in this direction only -- it is not a
/// primitive, it has no boxed payload, and the forward table is asked about boxing.
fn primitive_sig_name(sig: &SigType) -> Option<(&'static str, &'static str)> {
    let name = match sig {
        SigType::Boolean => "Boolean",
        SigType::Char => "Char",
        SigType::I1 => "SByte",
        SigType::U1 => "Byte",
        SigType::I2 => "Int16",
        SigType::U2 => "UInt16",
        SigType::I4 => "Int32",
        SigType::U4 => "UInt32",
        SigType::I8 => "Int64",
        SigType::U8 => "UInt64",
        SigType::R4 => "Single",
        SigType::R8 => "Double",
        SigType::IntPtr => "IntPtr",
        SigType::UIntPtr => "UIntPtr",
        SigType::String => "String",
        _ => return None,
    };
    Some(("System", name))
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
            let (arg_types, local_types) = slot_types(assembly, method, &target)?;
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
        let (arg_types, local_types) = slot_types(assembly, method, &target)?;
        let (func, map) = lower_method_typed(&body, &resolver, &arg_types, &local_types)?;
        funcs.push(func);
        maps.push(map);
    }
    Ok((funcs, maps))
}

/// A method's argument and local MIR types, from its signature and local-variable
/// signature; a type the backend does not lower yet falls back to `int32`.
///
/// Public so a DIAGNOSTIC types a method exactly as [`lower_methods_with_references`] does. A tool
/// that computed its own slot types would be reporting a different program from the one the build
/// lowers, which is the failure the MIR dump already paid for once by lowering without references.
///
/// **THE `int32` FALLBACK IS KEPT EVERYWHERE EXCEPT ONE SHAPE, AND THAT NARROWNESS IS DELIBERATE.**
/// `mir_type` answers `None` for `void`, `!n`, `!!n` and function pointers as well, and which of
/// those actually reach a slot in a real program is a measurement nobody has taken -- refusing them
/// all would trade one known silent wrong answer for an unknown loud one. So only the case that
/// provably miscompiles today refuses: an instantiation of a VALUE type, asked of
/// [`crate::generics::is_value_type_instantiation`] rather than spelled out here, because the build's own
/// typing path has to refuse the identical shape and two hand-written arms are how the pair drifted.
///
/// **A REFUSAL HERE IS HALF A FIX.** This function types what a diagnostic reads;
/// `build::mir_type` types what the image is emitted from. Landed here alone, the MIR dump becomes
/// honest and every image keeps miscompiling, with a unit test over this function green throughout.
pub fn slot_types(
    assembly: &Assembly,
    method: &Method,
    target: &TargetLayout,
) -> Result<(Vec<MirType>, Vec<MirType>), CilError> {
    let typed = |sig: &SigType| -> Result<MirType, CilError> {
        if crate::generics::is_value_type_instantiation(sig) {
            return instantiated_value_type_slot(sig, assembly, &[], target).ok_or_else(|| {
                CilError::GenericValueTypeSlot(
                    crate::generics::spell_sig(assembly, sig)
                        .unwrap_or_else(|| String::from("an unnameable value-type instantiation")),
                )
            });
        }
        Ok(mir_type(sig, assembly, target).unwrap_or(MirType::I32))
    };
    let mut arg_types = Vec::new();
    if let Some(signature) = method.signature() {
        if signature.has_this {
            arg_types.push(MirType::ManagedPtr);
        }
        for param in &signature.parameters {
            arg_types.push(typed(param)?);
        }
    }
    let local_types = method
        .local_variables()
        .iter()
        .map(&typed)
        .collect::<Result<Vec<MirType>, CilError>>()?;
    Ok((arg_types, local_types))
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
/// Units, not a [`String`], and that is the whole point of this function's shape. Ending in
/// `String::from_utf16_lossy` and handing back text, which the `ldstr` lowering then re-encodes to
/// UTF-16, is a round trip through a type that CANNOT HOLD a lone surrogate: `"a\u{D800}b"` reached every
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

    /// A fixture assembly from this crate's fixture directory, read at RUN time.
    ///
    /// **`include_bytes!` CANNOT BE USED HERE, AND THE REASON IS A DISTRIBUTION RULE RATHER THAN A
    /// TESTING ONE.** It resolves at COMPILE time, so a tree that does not carry the fixture
    /// directory fails to build this module at all -- while the identical source builds perfectly in
    /// a tree that does. Reading at run time compiles everywhere and runs wherever the fixture
    /// exists.
    ///
    /// **A MISSING FIXTURE SKIPS ONLY WHERE SKIPPING IS RIGHT.** If the DIRECTORY is gone -- exactly
    /// the stripped drop and nowhere else -- the caller returns early. If the directory exists and
    /// the FILE does not, that is a real breakage and this panics by name. Without that split,
    /// deleting a fixture would turn every row using it green.
    fn fixture_bytes(name: &str) -> Option<Vec<u8>> {
        let directory = alloc::format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        if !std::path::Path::new(&directory).is_dir() {
            eprintln!("{directory} absent (a stripped drop); skipping");
            return None;
        }
        let path = alloc::format!("{directory}/{name}");
        Some(std::fs::read(&path).unwrap_or_else(|error| {
            panic!("the fixture directory exists but {path} does not read: {error}")
        }))
    }

    /// THE OTHER HALF OF A CLAIM THAT NOW SPANS TWO CRATES, AND EACH HALF NAMES THE OTHER.
    /// `lamella_generics::tests::instantiations_are_distinct_interfaces` proves the canonical
    /// SPELLING separates two reference instantiations; this proves the INTERFACE TAG built from
    /// that spelling separates them too -- which is the property `o is IList<string>` answering
    /// FALSE for a type implementing only `IList<Foo>` actually rests on.
    ///
    /// **Two REFERENCE arguments, deliberately.** A pair like `IList<int>` against
    /// `IList<string>` passes under BOTH the by-name spelling and the collapsed one, because a
    /// VALUE argument is not what the shortcut collapses -- so it would separate nothing.
    #[test]
    fn an_instantiations_interface_tag_takes_the_canonical_spelling() {
        use lamella_generics::TypeArg;
        let Some(dll) = fixture_bytes("interface.dll") else {
            return;
        };
        let assembly = Assembly::read(&dll).expect("parse the fixture");
        let of = |argument: TypeArg| {
            TypeArg::Instance {
                definition: "System.Collections.Generic.IList`1"
                    .to_string()
                    .into_boxed_str(),
                value_type: false,
                arguments: alloc::vec![argument],
            }
            .name()
        };
        let string_name = of(TypeArg::Primitive(
            lamella_metadata::signature::element::STRING,
        ));
        let foo_name = of(TypeArg::Named {
            name: "Sample.Foo".to_string().into_boxed_str(),
            value_type: false,
        });
        let tag_of = |name: &str| {
            interface_method_tag(
                &assembly,
                &TypeName {
                    namespace: "System.Collections.Generic",
                    name: name.strip_prefix("System.Collections.Generic.").unwrap(),
                },
                "Add",
                &[],
            )
            .expect("a nullary parameter list names no type, so this cannot refuse")
        };
        assert_ne!(
            tag_of(&string_name),
            tag_of(&foo_name),
            "IList<string> and IList<Foo> must not share an interface tag"
        );
    }

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
            extern_method_symbol(
                "System",
                "Math",
                "Max",
                &[SigType::I4, SigType::I4],
                &SigType::I4,
                &none
            ),
            "System.Math.Max.ii.i"
        );
        assert_eq!(
            extern_method_symbol(
                "System",
                "Math",
                "Max",
                &[SigType::I8, SigType::I8],
                &SigType::I8,
                &none
            ),
            "System.Math.Max.ll.l"
        );
        assert_eq!(
            extern_method_symbol("", "MathLib", "Answer", &[], &SigType::I4, &none),
            "MathLib.Answer..i"
        );
        assert_eq!(
            extern_method_symbol("", "MathLib", "Reset", &[], &SigType::Void, &none),
            "MathLib.Reset..v"
        );
    }

    #[test]
    fn extern_symbol_separates_conversion_operators_that_differ_only_in_return_type() {
        let names = |t: Token| match t.0 {
            1 => Some(String::from("System.Device.Gpio.PinValue")),
            _ => None,
        };
        let pv = SigType::ValueType(Token(1));
        let symbol = |ret: SigType| {
            extern_method_symbol(
                "System.Device.Gpio",
                "PinValue",
                "op_Explicit",
                &[pv.clone()],
                &ret,
                &names,
            )
        };
        let to_byte = symbol(SigType::U1);
        let to_int = symbol(SigType::I4);
        let to_bool = symbol(SigType::Boolean);
        assert_ne!(to_byte, to_int, "byte and int conversions are distinct");
        assert_ne!(to_int, to_bool, "int and bool conversions are distinct");
        assert_ne!(to_byte, to_bool, "byte and bool conversions are distinct");
        assert_eq!(
            to_bool,
            "System.Device.Gpio.PinValue.op_Explicit.VSystem.Device.Gpio.PinValue;.z"
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
                &ts,
                &names
            ),
            "System.DateTime.op_Subtraction.VSystem.DateTime;VSystem.DateTime;.VSystem.TimeSpan;"
        );
        assert_eq!(
            extern_method_symbol(
                "System",
                "DateTime",
                "op_Subtraction",
                &[dt.clone(), ts.clone()],
                &dt,
                &names
            ),
            "System.DateTime.op_Subtraction.VSystem.DateTime;VSystem.TimeSpan;.VSystem.DateTime;"
        );
        assert_eq!(
            extern_method_symbol(
                "System",
                "Array",
                "Sort",
                &[SigType::SzArray(Box::new(SigType::I4))],
                &SigType::Void,
                &names
            ),
            "System.Array.Sort.ai.v"
        );
        assert_eq!(
            extern_method_symbol(
                "System",
                "Int32",
                "TryParse",
                &[SigType::String, SigType::ByRef(Box::new(SigType::I4))],
                &SigType::Boolean,
                &names
            ),
            "System.Int32.TryParse.qri.z"
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

    /// `primitive_sig_name` is the INVERSE of `primitive_sig_type`, walked rather than read.
    ///
    /// Two spellings of one table is the shape where a case gets added to one and not the other, and
    /// this lane has paid for that more than once. The loop is over the forward table's own names, so
    /// a primitive added there and forgotten here fails HERE -- which is the direction that matters,
    /// because the reverse map is what decides an array element's width and its collector kind.
    #[test]
    fn a_primitive_name_round_trips_through_both_tables() {
        let names = [
            "Boolean", "Char", "SByte", "Byte", "Int16", "UInt16", "Int32", "UInt32", "Int64",
            "UInt64", "Single", "Double", "IntPtr", "UIntPtr",
        ];
        for name in names {
            let sig = primitive_sig_type("System", name)
                .unwrap_or_else(|| panic!("{name} is in the forward table"));
            assert_eq!(
                primitive_sig_name(&sig),
                Some(("System", name)),
                "{name} must round-trip back to its own name"
            );
        }
        assert_eq!(
            primitive_sig_name(&SigType::String),
            Some(("System", "String"))
        );
        assert_eq!(primitive_sig_type("System", "String"), None);
        assert_eq!(primitive_sig_name(&SigType::Object), None);
        assert_eq!(
            primitive_sig_name(&SigType::SzArray(Box::new(SigType::I4))),
            None
        );
        assert_eq!(primitive_sig_name(&SigType::Var(0)), None);
    }

    /// THE DEFECT, STATED: `Pointer` and `ByRef` had no arm in `sig_element_byte` and fell to a
    /// `_ => 0x00` fallback, so three overloads that differ only in those parameter types produced
    /// ONE tag -- and a tag IS the dispatch key, so two of the three would have dispatched to the
    /// third's implementation. The `assert_ne!`s below are the defect: every one of them held as
    /// `assert_eq!` before the fallback was removed.
    #[test]
    fn pointer_and_byref_parameters_no_longer_collapse_to_one_tag() {
        let Some(dll) = fixture_bytes("interface.dll") else {
            return;
        };
        let assembly = Assembly::read(&dll).expect("parse the fixture");
        let iface = TypeName {
            namespace: "Probe",
            name: "IOverloads",
        };
        let tag = |p: SigType| {
            interface_method_tag(&assembly, &iface, "Bar", &[p])
                .expect("a non-generic parameter names no type, so this cannot refuse")
        };
        let int_ptr = tag(SigType::Pointer(Box::new(SigType::I4)));
        let byte_ptr = tag(SigType::Pointer(Box::new(SigType::U1)));
        let by_ref = tag(SigType::ByRef(Box::new(SigType::I4)));
        let plain = tag(SigType::I4);
        assert_ne!(int_ptr, by_ref, "T* and ref T are different signatures");
        assert_ne!(int_ptr, plain, "T* and T are different signatures");
        assert_ne!(by_ref, plain, "ref T and T are different signatures");
        assert_ne!(
            int_ptr, byte_ptr,
            "int* and byte* are different signatures -- PTR alone cannot name its pointee"
        );
    }

    /// THE ONLY TEST THAT CAN SEE THE TAG'S SPELLING CHANGE, and the reason it has to spell the
    /// answers out is the thing that makes this value ABI in the first place.
    ///
    /// Every other test here compares two tags this build computed, so both sides move together and
    /// a changed spelling keeps them all green. But the agreement the tag actually needs is across
    /// BUILDS, not within one: a `callvirt` emitted into a program object last month and the itable
    /// entry emitted into a library's type descriptor today must derive the same u32 from the same
    /// signature. **No test that recomputes the tag can observe that**, which is why the values
    /// below are literals and not expressions.
    ///
    /// DERIVATION, so this is an answer key rather than a mirror: FNV-1a-32 (offset basis
    /// `0x811c9dc5`, prime `0x01000193`) over the interface's namespace, `.`, its name, `.`, the
    /// method name, then per parameter its `II.23.1.16` element byte -- **and, for a byte that
    /// cannot name its own type, the canonical spelling after it** (`System.Int32*`,
    /// `System.Int32&`). The result has its high bit forced.
    ///
    /// **ANCHOR THE GENERATOR BEFORE POINTING IT AT THIS DATA**, against FNV-1a-32's own published
    /// check values -- `""` -> `0x811c9dc5`, `"a"` -> `0xe40c292c`, `"foobar"` -> `0xbf9cf968`.
    /// **A value copied out of the implementation because the independent derivation was fiddly is a
    /// mirror, and a mirror cannot fail.**
    ///
    /// A failure here is not a bug in this test. It means the tag's spelling moved, and every
    /// artifact built before the change now disagrees with every artifact built after it -- a
    /// deliberate decision while artifacts are ours to re-cut, and a break once they are not.
    #[test]
    fn the_interface_tag_spelling_is_pinned_to_literal_values() {
        let Some(dll) = fixture_bytes("interface.dll") else {
            return;
        };
        let assembly = Assembly::read(&dll).expect("parse the fixture");
        let tag = |interface: &TypeName, method: &str, params: &[SigType]| {
            interface_method_tag(&assembly, interface, method, params)
                .expect("a non-generic parameter names no type, so this cannot refuse")
        };
        let foo = TypeName {
            namespace: "",
            name: "IFoo",
        };
        let list = TypeName {
            namespace: "System.Collections",
            name: "IList",
        };
        let mut moved: Vec<String> = Vec::new();
        for (label, tag, expected) in [
            ("IFoo.Bar()", tag(&foo, "Bar", &[]), 0xaca5_33df),
            ("IFoo.Bar(int)", tag(&foo, "Bar", &[SigType::I4]), 0x9f10_9b75),
            (
                "IFoo.Bar(int*)",
                tag(&foo, "Bar", &[SigType::Pointer(Box::new(SigType::I4))]),
                0x81b8_4de7,
            ),
            (
                "IFoo.Bar(ref int)",
                tag(&foo, "Bar", &[SigType::ByRef(Box::new(SigType::I4))]),
                0xd8de_64ac,
            ),
            (
                "IFoo.Bar(int, string)",
                tag(&foo, "Bar", &[SigType::I4, SigType::String]),
                0xe224_c2a1,
            ),
            (
                "System.Collections.IList.Add(object)",
                tag(&list, "Add", &[SigType::Object]),
                0xe64e_7989,
            ),
        ] {
            if tag != expected {
                moved.push(alloc::format!("{label}: {expected:#010x} -> {tag:#010x}"));
            }
        }
        assert!(
            moved.is_empty(),
            "{} of 6 pinned tags moved -- this is cross-assembly ABI and every artifact built \
             before the change disagrees with every one built after:\n  {}",
            moved.len(),
            moved.join("\n  ")
        );
    }

    /// A GENERIC parameter contributes more than its element byte -- **and exactly how much more is
    /// the point of this test: the obvious reading is the wrong one, and this is what says so.**
    ///
    /// **THE SEPARATION IT ACHIEVES:** the ARGUMENTS. Folding `element::GENERICINST` alone -- the
    /// obvious reading of "give the new variants an arm" -- passes every other assertion in this
    /// file while making `List<int>` and `List<string>` ONE TAG. The first row is what tells those
    /// two candidates apart.
    ///
    /// **THE SEPARATION IT NOW ALSO ACHIEVES: THE DEFINITION, AND THE NAMED ARGUMENT.** A definition
    /// used to fold its KIND byte, so `List<int>` and `HashSet<int>` were one tag and so were
    /// `List<Foo>` and `List<Bar>`. An instantiation folds its CANONICAL SPELLING instead, which is
    /// the only identity that survives the assembly boundary -- a token is assembly-relative and
    /// therefore unusable in a cross-assembly tag. **Both of those rows were `assert_eq!` and are
    /// now `assert_ne!`; that inversion IS the change.**
    ///
    /// **THE TOKENS ARE READ FROM A REAL ASSEMBLY AND NOT BUILT HERE, AND THAT IS LOAD-BEARING.**
    /// The fold RESOLVES a token, so an invented one -- `Class(Token::new(0x01, 7))`, a row no
    /// assembly contains -- takes the refusal arm instead of the rule, and then asserting a COLLISION
    /// succeeds whether the rule is right or not. A fixture that builds its own input is green about
    /// a path nothing takes.
    ///
    /// **What is still NOT separated** is a bare `Class`/`ValueType` parameter: `IFoo.Bar(Foo)` and
    /// `IFoo.Bar(Bar)` collide, as do `IFoo.Bar(int[])` and `IFoo.Bar(string[])`. That imprecision is
    /// deliberate, pre-existing and recorded -- see [`sig_element_byte`] -- and closing it moves
    /// every shipped tag with a non-primitive parameter, which this change deliberately does not.
    #[test]
    fn a_generic_parameter_folds_its_canonical_spelling() {
        use lamella_metadata::signature::element;
        let Some(dll) = fixture_bytes("interface.dll") else {
            return;
        };
        let assembly = Assembly::read(&dll).expect("parse the fixture");
        let foo = TypeName {
            namespace: "",
            name: "IFoo",
        };
        let named =
            |n: &str| SigType::Class(assembly.find_type("", n).expect("fixture type").token());
        let list = named("IShape");
        let set = named("Square");
        let other = named("Circle");
        let instance = |definition: &SigType, argument: SigType| SigType::GenericInst {
            definition: Box::new(definition.clone()),
            arguments: alloc::vec![argument],
        };
        let tag = |p: SigType| {
            interface_method_tag(&assembly, &foo, "Bar", &[p])
                .expect("every token below resolves, so none of these refuse")
        };

        assert_ne!(
            tag(instance(&list, SigType::I4)),
            tag(instance(&list, SigType::String)),
            "List<int> and List<string> must not share a tag"
        );
        assert_ne!(tag(instance(&list, SigType::I4)), tag(list.clone()));
        assert_ne!(tag(instance(&list, SigType::I4)), tag(SigType::I4));
        assert_ne!(tag(SigType::Var(0)), tag(SigType::Var(1)));
        assert_ne!(tag(SigType::Var(0)), tag(SigType::MVar(0)));
        assert_ne!(element::VAR, element::MVAR);

        assert_ne!(
            tag(instance(&list, SigType::I4)),
            tag(instance(&set, SigType::I4)),
            "List<int> and HashSet<int> must not share a tag -- the definition folds its NAME"
        );
        assert_ne!(
            tag(instance(&list, set.clone())),
            tag(instance(&list, other.clone())),
            "List<Foo> and List<Bar> must not share a tag"
        );
        assert_ne!(
            tag(set.clone()),
            tag(other.clone()),
            "two bare Class parameters name different types and must not share a dispatch key"
        );
        assert_ne!(
            tag(SigType::SzArray(Box::new(SigType::I4))),
            tag(SigType::SzArray(Box::new(SigType::String))),
            "int[] and string[] are different signatures -- SZARRAY alone cannot name its element"
        );
        let ranked = |rank: u32| SigType::Array {
            element: Box::new(SigType::I4),
            rank,
        };
        assert_ne!(
            tag(ranked(2)),
            tag(ranked(3)),
            "int[,] and int[,,] are different signatures"
        );
    }

    /// **A PARAMETER THIS ASSEMBLY CANNOT NAME IS A REFUSAL, NOT A DEGRADATION**, and this is the
    /// row that says so: folding the bare `GENERICINST` byte instead would put two instantiations
    /// under one dispatch key, which is precisely the defect the spelling closes. A refusal a caller
    /// maps to a default is not a refusal, so the value is `None` and every call site skips.
    ///
    /// The control is the SAME instantiation with a resolvable definition, which must answer `Some`
    /// -- without it, a fold that refused unconditionally would pass.
    #[test]
    fn an_unnameable_instantiation_refuses_rather_than_folding_a_bare_byte() {
        let Some(dll) = fixture_bytes("interface.dll") else {
            return;
        };
        let assembly = Assembly::read(&dll).expect("parse the fixture");
        let foo = TypeName {
            namespace: "",
            name: "IFoo",
        };
        let instance = |definition: SigType| SigType::GenericInst {
            definition: Box::new(definition),
            arguments: alloc::vec![SigType::I4],
        };
        let unnameable = SigType::Class(Token::new(table::TYPE_DEF, 0x00ff_fffe));
        assert_eq!(
            interface_method_tag(&assembly, &foo, "Bar", &[instance(unnameable)]),
            None,
            "an instantiation whose definition cannot be named must refuse the tag"
        );
        let real = SigType::Class(assembly.find_type("", "IShape").expect("IShape").token());
        assert!(
            interface_method_tag(&assembly, &foo, "Bar", &[instance(real)]).is_some(),
            "the control: the same shape with a resolvable definition still yields a tag"
        );
    }

    /// THE ADDITIVE CLAIM, AS A GUARD RATHER THAN A SENTENCE. Every non-generic parameter folds
    /// exactly one byte, so no tag that exists today can move -- and the six pinned literals above
    /// are only half the proof, because they were written before `fold_tag_element` existed and a
    /// fold that appended a constant to EVERY parameter would break them loudly. This asserts the
    /// other half: the fold and the raw byte agree on every non-generic type this tier emits.
    #[test]
    fn a_primitive_parameter_folds_exactly_its_element_byte() {
        let Some(dll) = fixture_bytes("interface.dll") else {
            return;
        };
        let assembly = Assembly::read(&dll).expect("parse the fixture");
        let foo = TypeName {
            namespace: "",
            name: "IFoo",
        };
        for ty in [
            SigType::Void,
            SigType::Boolean,
            SigType::Char,
            SigType::I1,
            SigType::U1,
            SigType::I2,
            SigType::U2,
            SigType::I4,
            SigType::U4,
            SigType::I8,
            SigType::U8,
            SigType::R4,
            SigType::R8,
            SigType::String,
            SigType::Object,
            SigType::IntPtr,
            SigType::UIntPtr,
            SigType::TypedByRef,
        ] {
            let folded = fold_tag_element(&assembly, 0x811c_9dc5, &ty);
            let raw = fnv1a32(0x811c_9dc5, &[sig_element_byte(&ty)]);
            assert_eq!(folded, Some(raw), "{ty:?} must fold exactly one byte");
            assert_eq!(
                interface_method_tag(&assembly, &foo, "Bar", &[ty.clone()]),
                Some(
                    fnv1a32(
                        fnv1a32(fnv1a32(fnv1a32(0x811c_9dc5, b"IFoo"), b"."), b"Bar"),
                        &[sig_element_byte(&ty)]
                    ) | 0x8000_0000
                )
            );
        }
    }

    /// **THE OTHER HALF, AND THE ONE THAT MOVED THE ABI.** A byte standing for an infinite family
    /// -- `CLASS`, `VALUETYPE`, `PTR`, `BYREF`, `SZARRAY`, `ARRAY`, `GENERICINST` -- folds the
    /// canonical spelling after it, because the byte alone puts distinct legal signatures under one
    /// dispatch key.
    ///
    /// **THE ROWS ARE PAIRS THAT SHARE A BYTE, NOT SINGLE TYPES, AND THAT IS THE WHOLE
    /// CONSTRUCTION.** Asserting "this folds more than one byte" would pass under a fold that
    /// appended any constant -- including one appending the SAME constant to every parameter, which
    /// separates nothing. Each row here differs only in the part the byte cannot express, so only a
    /// fold that reads that part can tell them apart.
    #[test]
    fn a_parameter_whose_byte_cannot_name_its_type_folds_the_spelling() {
        let Some(dll) = fixture_bytes("interface.dll") else {
            return;
        };
        let assembly = Assembly::read(&dll).expect("parse the fixture");
        let foo = TypeName {
            namespace: "",
            name: "IFoo",
        };
        let named =
            |n: &str| assembly.find_type("", n).expect("fixture type").token();
        let (a, b) = (named("Square"), named("Circle"));
        let tag = |p: SigType| {
            interface_method_tag(&assembly, &foo, "Bar", &[p])
                .expect("every token below resolves")
        };
        let boxed = |t: SigType| Box::new(t);
        for (label, left, right) in [
            ("CLASS", SigType::Class(a), SigType::Class(b)),
            ("VALUETYPE", SigType::ValueType(a), SigType::ValueType(b)),
            (
                "PTR",
                SigType::Pointer(boxed(SigType::I4)),
                SigType::Pointer(boxed(SigType::U1)),
            ),
            (
                "BYREF",
                SigType::ByRef(boxed(SigType::I4)),
                SigType::ByRef(boxed(SigType::String)),
            ),
            (
                "SZARRAY",
                SigType::SzArray(boxed(SigType::I4)),
                SigType::SzArray(boxed(SigType::String)),
            ),
            (
                "ARRAY rank",
                SigType::Array {
                    element: boxed(SigType::I4),
                    rank: 2,
                },
                SigType::Array {
                    element: boxed(SigType::I4),
                    rank: 3,
                },
            ),
        ] {
            assert_eq!(
                sig_element_byte(&left),
                sig_element_byte(&right),
                "{label}: the PRECONDITION -- these two must share an element byte, or the row \
                 below proves nothing about the spelling"
            );
            assert_ne!(
                tag(left),
                tag(right),
                "{label}: two signatures sharing one element byte must not share a dispatch key"
            );
        }
    }

    /// **`slot_key`'s DISTINGUISHER MUST BE DISJOINT FROM EVERY REAL KEY, AND THAT IS THE HALF OF
    /// IT A TEST CAN REACH.** The undecodable arm itself is unreachable in this tree -- the
    /// population is measured at zero since generic signatures started decoding -- so there is no
    /// fixture that exercises it. What CAN fail silently is the disjointness: someone adding `#` to
    /// [`encode_type`]'s alphabet would make a real signature collide with a distinguisher, and the
    /// wrong-bind would be back with nothing red.
    ///
    /// **The nullary row is the one that matters most.** A method with no parameters once keyed the
    /// EMPTY string, which is also what a substituted-away undecodable method produces -- so the
    /// distinguisher must differ from the empty key too, not merely from populated ones.
    #[test]
    fn no_real_parameter_key_can_collide_with_the_undecodable_distinguisher() {
        let Some(dll) = fixture_bytes("interface.dll") else {
            return;
        };
        let assembly = Assembly::read(&dll).expect("parse the fixture");
        let named = |n: &str| assembly.find_type("", n).expect("fixture type").token();
        let boxed = |t: SigType| Box::new(t);
        for params in [
            alloc::vec![],
            alloc::vec![SigType::I4],
            alloc::vec![SigType::String, SigType::Object],
            alloc::vec![SigType::Class(named("Square"))],
            alloc::vec![SigType::ValueType(named("Circle"))],
            alloc::vec![SigType::SzArray(boxed(SigType::I4))],
            alloc::vec![SigType::Array {
                element: boxed(SigType::I4),
                rank: 3,
            }],
            alloc::vec![SigType::Pointer(boxed(SigType::U1))],
            alloc::vec![SigType::ByRef(boxed(SigType::String))],
            alloc::vec![SigType::Var(0), SigType::MVar(1)],
        ] {
            let key = param_key(&assembly, 0, &params);
            assert!(
                !key.starts_with('#'),
                "a real parameter key must never begin with the undecodable distinguisher: \
                 {params:?} keyed {key:?}"
            );
            assert_ne!(
                param_key(&assembly, 0, &params),
                param_key(&assembly, 1, &params),
                "arity 0 and arity 1 must not key alike for {params:?}"
            );
        }
        assert_ne!(param_key(&assembly, 0, &[]), alloc::format!("#{}", 3u32));
        assert_ne!(param_key(&assembly, 1, &[]), alloc::format!("#{}", 3u32));
    }

    /// The three codes reserved for generics must not alias a byte the tag already emits, or the
    /// first generic signature would be indistinguishable from an existing overload.
    #[test]
    fn the_reserved_generic_element_codes_alias_nothing_the_tag_emits() {
        use lamella_metadata::signature::element;
        let emitted = [
            SigType::Void,
            SigType::Boolean,
            SigType::Char,
            SigType::I1,
            SigType::U1,
            SigType::I2,
            SigType::U2,
            SigType::I4,
            SigType::U4,
            SigType::I8,
            SigType::U8,
            SigType::R4,
            SigType::R8,
            SigType::String,
            SigType::Pointer(Box::new(SigType::I4)),
            SigType::ByRef(Box::new(SigType::I4)),
            SigType::ValueType(lamella_token::Token::new(2, 1)),
            SigType::Class(lamella_token::Token::new(2, 1)),
            SigType::Array {
                element: Box::new(SigType::I4),
                rank: 2,
            },
            SigType::TypedByRef,
            SigType::IntPtr,
            SigType::UIntPtr,
            SigType::Object,
            SigType::SzArray(Box::new(SigType::I4)),
        ];
        let used: Vec<u8> = emitted.iter().map(sig_element_byte).collect();
        for reserved in [element::VAR, element::GENERICINST, element::MVAR] {
            assert!(
                !used.contains(&reserved),
                "reserved generic code {reserved:#04x} already names an emitted parameter kind"
            );
        }
        let mut sorted = used.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), used.len(), "two parameter kinds share one byte");
    }
}
