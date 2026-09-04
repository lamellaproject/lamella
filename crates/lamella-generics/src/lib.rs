#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The closed generic instantiation set a program uses, and the canonical spelling that names
//! each instantiation.

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use lamella_cil::Operand;
use lamella_ir::TypeHandle;
use lamella_metadata::signature::element;
use lamella_metadata::tables::table;
use lamella_metadata::{
    Assembly, CodedIndex, Method, SigType, exception_tag_for_name, fnv1a32, parse_local_vars,
    parse_method, parse_method_spec,
};
use lamella_token::Token;

use lamella_metadata::signature::element_byte as sig_element_byte;

/// A backstop on the recursion depth of the closure walk. It is NOT the refusal criterion --
/// [`Refusal::GrowthOnCycle`] is, and a bare depth cap is explicitly not equivalent to it, because a
/// cap rejects legal programs that merely nest deeply. Growth-on-a-cycle fires at the first strictly
/// deeper revisit, and the finite case's path is bounded by the number of distinct instantiations,
/// so this is unreachable unless the walk itself is wrong. It exists so a walk bug is a named
/// refusal instead of a blown stack.
const PATH_BACKSTOP: usize = 128;

/// A type as a monomorphizer must see it: a tree, with generic definitions named rather than
/// tokenized.
///
/// **NAMES, NOT TOKENS, AND THAT IS THE POINT.** A metadata token is meaningful only inside its
/// own assembly, so the same token number in a program and in its corlib are different types. The
/// canonical name is the only identity that survives the assembly boundary -- which is the same
/// reason it is the interface tag's spelling, and the reason it is this set's key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TypeArg {
    /// A built-in element type carried as its ECMA-335 element byte (II.23.1.16) -- `I4`, `STRING`,
    /// `OBJECT`, `I`, `TYPEDBYREF` and the rest of the payload-free bytes.
    Primitive(u8),
    /// A named non-generic type: `CLASS` or `VALUETYPE` followed by a `TypeDefOrRef`.
    Named {
        /// The type's full name, nested chain and namespace included.
        name: Box<str>,
        /// Whether it was spelled `VALUETYPE` rather than `CLASS`.
        value_type: bool,
    },
    /// A constructed generic type: `GENERICINST (CLASS|VALUETYPE) <TypeDefOrRef> GenArgCount Type*`.
    Instance {
        /// The generic definition's full name, backtick arity included (`` List`1 ``).
        definition: Box<str>,
        /// Whether the definition is a value type.
        value_type: bool,
        /// The type arguments, in declaration order.
        arguments: Vec<TypeArg>,
    },
    /// `!n` -- a type parameter of the enclosing TYPE.
    Var(u32),
    /// `!!n` -- a type parameter of the enclosing METHOD.
    MVar(u32),
    /// `T[]`.
    SzArray(Box<TypeArg>),
    /// `T[,]` and wider. The bounds and sizes a signature may carry do not name a type, so only the
    /// rank is kept.
    Array {
        /// The element type.
        element: Box<TypeArg>,
        /// The number of dimensions.
        rank: u32,
    },
    /// `T*`.
    Pointer(Box<TypeArg>),
    /// `ref T`.
    ByRef(Box<TypeArg>),
}

impl TypeArg {
    /// Whether this type mentions no type parameter anywhere inside it -- the property that makes an
    /// instantiation emittable.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        match self {
            TypeArg::Var(_) | TypeArg::MVar(_) => false,
            TypeArg::Primitive(_) | TypeArg::Named { .. } => true,
            TypeArg::Instance { arguments, .. } => arguments.iter().all(TypeArg::is_closed),
            TypeArg::SzArray(inner)
            | TypeArg::Array { element: inner, .. }
            | TypeArg::Pointer(inner)
            | TypeArg::ByRef(inner) => inner.is_closed(),
        }
    }

    /// The type-argument NESTING depth: 0 for a leaf, and one more than its deepest argument for an
    /// instantiation. This is the quantity the growth-on-a-cycle criterion compares, so
    /// `C<C<int>>` (2) is deeper than `C<int>` (1) while `C<int>` and `C<string>` are equal.
    #[must_use]
    pub fn depth(&self) -> u32 {
        match self {
            TypeArg::Primitive(_)
            | TypeArg::Named { .. }
            | TypeArg::Var(_)
            | TypeArg::MVar(_) => 0,
            TypeArg::Instance { arguments, .. } => {
                1 + arguments.iter().map(TypeArg::depth).max().unwrap_or(0)
            }
            TypeArg::SzArray(inner)
            | TypeArg::Array { element: inner, .. }
            | TypeArg::Pointer(inner)
            | TypeArg::ByRef(inner) => inner.depth(),
        }
    }

    /// This type with `!n` replaced by `type_args[n]` and `!!n` by `method_args[n]`.
    ///
    /// `None` when a parameter number has no argument -- a signature referring to `!3` of a
    /// two-parameter type is not something to substitute a default into. That is the same rule the
    /// undecodable-signature guard follows: a refusal a caller maps to a default is not a refusal.
    #[must_use]
    pub fn substitute(&self, type_args: &[TypeArg], method_args: &[TypeArg]) -> Option<TypeArg> {
        Some(match self {
            TypeArg::Var(n) => type_args.get(*n as usize)?.clone(),
            TypeArg::MVar(n) => method_args.get(*n as usize)?.clone(),
            TypeArg::Primitive(_) | TypeArg::Named { .. } => self.clone(),
            TypeArg::Instance {
                definition,
                value_type,
                arguments,
            } => TypeArg::Instance {
                definition: definition.clone(),
                value_type: *value_type,
                arguments: arguments
                    .iter()
                    .map(|argument| argument.substitute(type_args, method_args))
                    .collect::<Option<Vec<_>>>()?,
            },
            TypeArg::SzArray(inner) => {
                TypeArg::SzArray(Box::new(inner.substitute(type_args, method_args)?))
            }
            TypeArg::Array { element, rank } => TypeArg::Array {
                element: Box::new(element.substitute(type_args, method_args)?),
                rank: *rank,
            },
            TypeArg::Pointer(inner) => {
                TypeArg::Pointer(Box::new(inner.substitute(type_args, method_args)?))
            }
            TypeArg::ByRef(inner) => {
                TypeArg::ByRef(Box::new(inner.substitute(type_args, method_args)?))
            }
        })
    }

    /// The canonical spelling of this type, appended to `out`.
    ///
    /// See the module documentation for the shape and where it came from. An open type spells its
    /// parameter as `!n` / `!!n`, which is deliberately NOT a legal instantiation name -- an open
    /// type never reaches the tag, and a spelling that silently looked closed would be the shape
    /// that puts two types under one tag.
    pub fn spell(&self, out: &mut String) {
        match self {
            TypeArg::Primitive(byte) => out.push_str(primitive_name(*byte)),
            TypeArg::Named { name, .. } => out.push_str(name),
            TypeArg::Instance {
                definition,
                arguments,
                ..
            } => {
                out.push_str(definition);
                out.push('[');
                for (index, argument) in arguments.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    argument.spell(out);
                }
                out.push(']');
            }
            TypeArg::Var(n) => out.push_str(&format!("!{n}")),
            TypeArg::MVar(n) => out.push_str(&format!("!!{n}")),
            TypeArg::SzArray(inner) => {
                inner.spell(out);
                out.push_str("[]");
            }
            TypeArg::Array { element, rank } => {
                element.spell(out);
                out.push('[');
                for _ in 1..*rank {
                    out.push(',');
                }
                out.push(']');
            }
            TypeArg::Pointer(inner) => {
                inner.spell(out);
                out.push('*');
            }
            TypeArg::ByRef(inner) => {
                inner.spell(out);
                out.push('&');
            }
        }
    }

    /// The canonical spelling of this type as an owned string -- [`spell`](Self::spell) for a caller
    /// that wants the whole name rather than a piece of one.
    #[must_use]
    pub fn name(&self) -> String {
        let mut out = String::new();
        self.spell(&mut out);
        out
    }
}

/// The table byte a SYNTHESIZED instantiation's [`TypeHandle`] rides.
///
/// A handle is `TypeHandle(token.0)` -- a table byte over a row -- and an instantiation has no
/// metadata row at all (the resolver reads a READ-ONLY `Assembly`), so it takes a byte no type
/// table occupies and carries a value derived from its NAME instead of a row number.
///
/// **THE BYTE IS ALLOCATED IN `lamella-ir` AND RE-EXPORTED HERE, NOT DECLARED HERE.** The handle
/// space is shared with every front end that mints a synthesized identity, so a byte chosen against
/// this tier's view of it is a byte chosen against half the population -- which is how an
/// instantiation and a front-end synthesized array came to share `0x04`, and a descriptor is
/// deduplicated BY HANDLE.
///
/// **AND THE CEILING THAT ARGUMENT WAS MADE AGAINST WAS NEVER REAL:** the
/// handle space is ALREADY past bit 27 -- a TypeSpec handle is table byte `0x1B` and sets bits 28 and
/// 27, in every rank-N array in shipping code, with both backends correct anyway. So `0x04` was not
/// chosen from a full space; it was chosen against a stale census.
pub use lamella_ir::INSTANTIATION_HANDLE_TABLE;

/// The handle for the instantiation spelled `name`.
///
/// **DERIVED FROM THE NAME, BECAUSE THE NAME IS THE IDENTITY AND A ROW NUMBER IS NOT.** The same
/// instantiation named from a program and from a library must get the SAME handle, or the two
/// assemblies emit two descriptors for one type and `o is Box<int>` stops working across the
/// boundary. A per-assembly row number cannot do that; the canonical spelling can, because it is
/// already the thing both sides agree on.
///
/// **AND 24 BITS OF HASH CAN COLLIDE, SO A COLLISION IS A REFUSAL RATHER THAN A RISK.** Two
/// distinct instantiations sharing a handle share a DESCRIPTOR -- two types under one tag, which is
/// the precise failure the by-name rule exists to prevent, arriving through the allocator instead
/// of through a cast. [`Program::instantiations`] checks the whole set and refuses by name; see
/// [`Refusal::HandleCollision`].
///
/// **The check protects a build that RUNS THE COLLECTOR.** Emission does not run it yet, so a
/// build that mints a handle through `lamella_aot::resolver` alone is currently unchecked. That is a
/// named gap, not a silent one, and it closes when the collector joins the build path.
#[must_use]
pub fn instantiation_handle(name: &str) -> TypeHandle {
    TypeHandle((INSTANTIATION_HANDLE_TABLE << 24) | (exception_tag_for_name("", name) & 0x00ff_ffff))
}

/// `ty` with `!n` replaced by `arguments[n]`, as a [`SigType`] rather than a [`TypeArg`].
///
/// # Why this exists beside [`TypeArg::substitute`], which does the same thing to a different type
///
/// [`TypeArg`] is the SET's currency: name-keyed, assembly-independent, built to be an identity.
/// `SigType` is the LAYOUT's currency -- it is what `layout_value_type` sizes, and sizing is where
/// substitution has to happen for an instantiation to have a shape at all. Converting a `TypeArg`
/// back to a `SigType` is not possible without inventing tokens, so the two directions are separate
/// functions over separate types rather than one function with a conversion in it.
///
/// **A METHOD parameter (`!!n`) resolves from the METHOD's arguments, which are a separate list.**
/// This form passes an empty one, so every `!!n` answers `None` here exactly as it always has --
/// see [`substitute_sig_with`] for why that is now a consequence of the rule rather than a rule of
/// its own.
///
/// `None` when a parameter number has no argument, for the same reason
/// [`TypeArg::substitute`] refuses: a signature naming `!3` of a two-parameter type is not something
/// to substitute a default into.
#[must_use]
pub fn substitute_sig(ty: &SigType, arguments: &[SigType]) -> Option<SigType> {
    substitute_sig_with(ty, arguments, &[])
}

/// `ty` with `!n` replaced from `type_arguments` and `!!n` from `method_arguments`.
///
/// **TWO LISTS, BECAUSE THERE ARE TWO AXES AND ECMA-335 SPELLS THEM WITH DIFFERENT BYTES.** A
/// TYPE's arguments come from the `TypeSpec` naming the instantiation; a generic METHOD's come from
/// a `MethodSpec` AT THE CALL SITE, and no amount of looking at the enclosing type will produce
/// them. Collapsing the two into one list would make `Pick<int>` inside `Box<string>` resolve `!!0`
/// to `string`, which is a wrong answer rather than a missing one.
///
/// **THE OLD UNCONDITIONAL `!!n` REFUSAL SURVIVES AS A CONSEQUENCE, NOT AS A SPECIAL CASE.** The
/// type axis calls [`substitute_sig`], which passes an EMPTY method list, so `method_arguments.get`
/// answers `None` for every `!!n` exactly as the hard-coded arm did. That is deliberate: the runtime
/// tier reached the same arrangement independently in its own loader, and having the two tiers agree
/// by construction is worth more than either being clever.
///
/// **Silently leaving `!!0` in place is the failure this refuses.** It would produce a type that
/// looks closed and is not, and it would then be laid out as whatever the layout code makes of an
/// unresolved parameter -- a size and a trace map invented from nothing.
#[must_use]
pub fn substitute_sig_with(
    ty: &SigType,
    type_arguments: &[SigType],
    method_arguments: &[SigType],
) -> Option<SigType> {
    let recur = |inner: &SigType| substitute_sig_with(inner, type_arguments, method_arguments);
    Some(match ty {
        SigType::Var(number) => type_arguments.get(*number as usize)?.clone(),
        SigType::MVar(number) => method_arguments.get(*number as usize)?.clone(),
        SigType::GenericInst {
            definition,
            arguments: inner,
        } => SigType::GenericInst {
            definition: Box::new(recur(definition)?),
            arguments: inner.iter().map(recur).collect::<Option<Vec<_>>>()?,
        },
        SigType::Pointer(inner) => SigType::Pointer(Box::new(recur(inner)?)),
        SigType::ByRef(inner) => SigType::ByRef(Box::new(recur(inner)?)),
        SigType::SzArray(inner) => SigType::SzArray(Box::new(recur(inner)?)),
        SigType::Array { element, rank } => SigType::Array {
            element: Box::new(recur(element)?),
            rank: *rank,
        },
        other => other.clone(),
    })
}

/// Converts a decoded [`SigType`] into a [`TypeArg`], resolving every token to a NAME.
///
/// **THIS IS WHERE THE TIER'S IDENTITY CHANGES FROM A TOKEN TO A NAME, AND IT IS THE WHOLE REASON
/// [`TypeArg`] EXISTS RATHER THAN `SigType` BEING USED DIRECTLY.** A token is meaningful only inside
/// its own assembly, so `SigType::Class(0x01000004)` from a program and the same number from its
/// corlib are different types. The canonical name is the only identity that survives the assembly
/// boundary -- which is also why it is the interface tag's spelling and the instantiation set's key.
///
/// It takes ONE assembly because that is all it needs: a `TypeRef` row carries its own namespace and
/// name, so naming never requires the defining assembly to be loaded.
pub fn sig_to_type_arg(assembly: &Assembly<'_>, ty: &SigType) -> Result<TypeArg, Refusal> {
    let named = |token: Token| -> Result<Box<str>, Refusal> {
        type_def_full_name(assembly, token)
            .map(String::into_boxed_str)
            .ok_or_else(|| undecodable("type name"))
    };
    Ok(match ty {
        SigType::Var(number) => TypeArg::Var(*number),
        SigType::MVar(number) => TypeArg::MVar(*number),
        SigType::GenericInst {
            definition,
            arguments,
        } => {
            let (token, value_type) = match definition.as_ref() {
                SigType::Class(token) => (*token, false),
                SigType::ValueType(token) => (*token, true),
                _ => return Err(undecodable("GenericInst definition")),
            };
            let mut decoded = Vec::new();
            for argument in arguments {
                decoded.push(sig_to_type_arg(assembly, argument)?);
            }
            TypeArg::Instance {
                definition: named(token)?,
                value_type,
                arguments: decoded,
            }
        }
        SigType::Class(token) | SigType::ValueType(token) => {
            if token.table() == table::TYPE_SPEC {
                let sig = assembly
                    .type_spec_signature(*token)
                    .ok_or_else(|| undecodable("TypeSpec"))?;
                sig_to_type_arg(assembly, &sig)?
            } else {
                TypeArg::Named {
                    name: named(*token)?,
                    value_type: matches!(ty, SigType::ValueType(_)),
                }
            }
        }
        SigType::Pointer(inner) => TypeArg::Pointer(Box::new(sig_to_type_arg(assembly, inner)?)),
        SigType::ByRef(inner) => TypeArg::ByRef(Box::new(sig_to_type_arg(assembly, inner)?)),
        SigType::SzArray(inner) => TypeArg::SzArray(Box::new(sig_to_type_arg(assembly, inner)?)),
        SigType::Array { element, rank } => TypeArg::Array {
            element: Box::new(sig_to_type_arg(assembly, element)?),
            rank: *rank,
        },
        other => TypeArg::Primitive(sig_element_byte(other)),
    })
}

/// The INVERSE of [`sig_to_type_arg`]: a name-keyed [`TypeArg`] back to a signature the assembly
/// being lowered could have written itself.
///
/// **RESOLVING A NAME IS NOT INVENTING A TOKEN, AND THAT DISTINCTION IS THE WHOLE REASON THIS IS
/// SOUND.** The closure walk hands back NAMES, because a token means nothing outside the assembly
/// that issued it. This looks each name up in `tokens` -- an index of the target assembly's OWN
/// `TypeDef` and `TypeRef` rows -- and takes the token that assembly ALREADY HAS. A `List<Alpha>`
/// closes over `ListEnumerator<Alpha>`; `Alpha` is the program's own type at the program's own row,
/// so the signature this builds is one the program could have spelled.
///
/// `None` the moment a part cannot be expressed -- an unresolvable name, or a `!n`/`!!n`, which is
/// an OPEN type and names no instantiation at all. Declining is the safe direction: the caller
/// leaves the call site refused rather than putting a fabricated type in an image.
///
/// A primitive carries no token anywhere (`int`, `string`, `object` are element BYTES, II.23.1.16),
/// which is why the whole BCL case -- `List<int>`, `EqualityComparer<int>` -- needs no `tokens`
/// entry at all.
pub fn type_arg_to_sig(
    argument: &TypeArg,
    tokens: &BTreeMap<String, Token>,
) -> Option<SigType> {
    let named = |name: &str, value_type: bool| -> Option<Token> {
        let _ = value_type;
        tokens.get(name).copied()
    };
    match argument {
        TypeArg::Primitive(byte) => lamella_metadata::signature::payload_free_sig(*byte),
        TypeArg::Named { name, value_type } => {
            let token = named(name, *value_type)?;
            Some(if *value_type {
                SigType::ValueType(token)
            } else {
                SigType::Class(token)
            })
        }
        TypeArg::Instance {
            definition,
            value_type,
            arguments,
        } => {
            let token = named(definition, *value_type)?;
            let inner: Option<Vec<SigType>> = arguments
                .iter()
                .map(|a| type_arg_to_sig(a, tokens))
                .collect();
            Some(SigType::GenericInst {
                definition: Box::new(if *value_type {
                    SigType::ValueType(token)
                } else {
                    SigType::Class(token)
                }),
                arguments: inner?,
            })
        }
        TypeArg::SzArray(element) => Some(SigType::SzArray(Box::new(type_arg_to_sig(
            element, tokens,
        )?))),
        TypeArg::Var(_) | TypeArg::MVar(_) => None,
        TypeArg::Array { .. } | TypeArg::Pointer(_) | TypeArg::ByRef(_) => None,
    }
}

/// The canonical spelling of a `SigType`, through [`TypeArg::spell`].
///
/// It exists so a caller holding a `SigType` never formats one itself. **A second spelling of a
/// frozen identity is the hazard this prevents** -- two implementations that agree today and
/// diverge on the first nested or array argument.
#[must_use]
pub fn spell_sig(assembly: &Assembly<'_>, ty: &SigType) -> Option<String> {
    sig_to_type_arg(assembly, ty).ok().map(|arg| arg.name())
}

/// The canonical spelling of an OPEN signature whose `!n` are filled from a DIFFERENT assembly --
/// the one shape where a definition and its type arguments are written in two different worlds.
///
/// **A MONOMORPHIZED BODY DECLARED NEXT DOOR IS THE CASE.** Its CIL is the OWNER's, so a `TypeSpec`
/// in it names the owner's definition; the instantiation's type arguments are the CALLER's, because
/// the caller is what spelled the instantiation. Substituting first and spelling second reads BOTH
/// through one assembly, and whichever one is wrong gets a name out of the other's tables -- a real,
/// unrelated type, and a lookup that misses or, worse, hits.
///
/// So each side is decoded in its own world FIRST and composed after: `definition` through
/// `definition_assembly`, every argument through `argument_assembly`, then substituted. It is
/// [`spell_sig`] for the ordinary one-world case and stays that, byte for byte, when the two
/// assemblies are the same.
///
/// `None` when either side does not decode, which is [`spell_sig`]'s own refusal rather than a
/// second rule: a spelling that cannot be produced exactly must not be produced approximately.
#[must_use]
pub fn spell_sig_across(
    definition_assembly: &Assembly<'_>,
    argument_assembly: &Assembly<'_>,
    open: &SigType,
    arguments: &[SigType],
) -> Option<String> {
    let definition = sig_to_type_arg(definition_assembly, open).ok()?;
    let decoded: Vec<TypeArg> = arguments
        .iter()
        .map(|argument| sig_to_type_arg(argument_assembly, argument).ok())
        .collect::<Option<_>>()?;
    Some(definition.substitute(&decoded, &[])?.name())
}

/// Whether a signature instantiates a VALUE type -- `Holder<int>`, never `List<int>`.
///
/// **ONE PREDICATE, BECAUSE TWO SITES REFUSE ON IT AND THEY MUST REFUSE ON THE SAME SHAPE.** The
/// AOT types a method's slots twice -- once in the resolver, for what a diagnostic reads, and once
/// inline in the build, for what the image is emitted from -- and each has to reject this shape.
/// Written out at both, they are two hand-kept-in-step arms, which is exactly the arrangement that
/// let the defect sit: the pair already carried *"must stay in step"* comments and had drifted
/// anyway.
///
/// The distinction is not cosmetic. An instantiation of a CLASS is an object reference whatever its
/// arguments are, so it is answerable with no layout at all. An instantiation of a VALUE type
/// carries a SIZE and a trace map that only the substituted layout can supply, and until this tier
/// monomorphizes value types it has neither -- so the honest answer is a refusal rather than a
/// guess one field wide.
///
/// Keyed POSITIVELY on `ValueType`, not as "anything that is not a `Class`". `GENERICINST` is
/// followed by exactly one of those two element bytes (ECMA-335 II.23.2.12), so the two readings
/// agree on every well-formed blob -- and on a malformed one the positive form leaves behavior
/// where it is instead of turning a decoding gap into a build failure.
#[must_use]
pub fn is_value_type_instantiation(ty: &SigType) -> bool {
    matches!(ty, SigType::GenericInst { definition, .. } if matches!(**definition, SigType::ValueType(_)))
}

/// The type arguments a `TypeSpec` token instantiates its definition with, and the definition's own
/// name -- the pair a caller needs to find the definition and substitute into it.
///
/// `None` when the token is not a `TypeSpec`, or its blob is not a `GENERICINST` (an array or
/// pointer `TypeSpec` is a perfectly ordinary thing that this is simply not about).
#[must_use]
pub fn instantiation_of(assembly: &Assembly<'_>, token: Token) -> Option<(String, Vec<SigType>)> {
    let SigType::GenericInst {
        definition,
        arguments,
    } = assembly.type_spec_signature(token)?
    else {
        return None;
    };
    let definition_token = match definition.as_ref() {
        SigType::Class(token) | SigType::ValueType(token) => *token,
        _ => return None,
    };
    Some((type_def_full_name(assembly, definition_token)?, arguments))
}

/// A fingerprint of the SPELLING RULE, so two artifacts built at different times can tell whether
/// they agree about what an instantiation is called.
///
/// # Why this exists, and why a version NUMBER would not do
///
/// The interpreter's loader interns a type by `(namespace, name)`, which is what lets a baked image
/// and a separately-loaded PE share one identity space. It also means that **if the two sides spell
/// `List<int>` differently by one character, they are two types where there should be one** -- a
/// cast that fails, a static field that exists twice, an `is` that answers wrong. Today one codebase
/// produces both sides so the agreement is accidental; the moment a device baked by one toolchain
/// loads a PE instantiated by another, the spelling is a WIRE CONTRACT.
///
/// **IT IS DERIVED, NOT DECLARED, AND THAT IS THE WHOLE POINT.** A hand-maintained
/// `SPELLING_VERSION` constant is a twin of the thing it describes, and this project has already
/// been bitten by a hand-maintained twin drifting from its original. This hashes what the rule
/// actually PRODUCES over a corpus with one entry per clause of the rule, so **changing a separator,
/// an argument order, an arity suffix, a nesting join, a compound suffix or a primitive's BCL name
/// moves the fingerprint whether or not anyone remembers to bump it.** Forgetting is not available.
///
/// **The corpus below is the specification.** A clause with no entry here is a clause this
/// fingerprint does not cover, so an addition to the rule needs a matching addition to the corpus --
/// [`tests::the_spelling_rule_fingerprint_is_pinned`] fails loudly when the value moves, which is
/// the prompt to check that the move was intended and to say so where consumers can see it.
#[must_use]
pub fn spelling_rule_fingerprint() -> u32 {
    let named = |name: &str| TypeArg::Named {
        name: name.to_owned().into_boxed_str(),
        value_type: false,
    };
    let instance = |definition: &str, arguments: Vec<TypeArg>| TypeArg::Instance {
        definition: definition.to_owned().into_boxed_str(),
        value_type: false,
        arguments,
    };
    let int = TypeArg::Primitive(element::I4);
    let corpus = alloc::vec![
        instance("N.List`1", alloc::vec![int.clone()]),
        instance(
            "N.Pair`2",
            alloc::vec![int.clone(), TypeArg::Primitive(element::STRING)]
        ),
        instance("N.List`1", alloc::vec![instance("N.List`1", alloc::vec![int.clone()])]),
        instance("N.Outer`1+Inner`1", alloc::vec![int.clone(), named("N.Foo")]),
        instance("N.List`1", alloc::vec![TypeArg::SzArray(Box::new(int.clone()))]),
        instance(
            "N.List`1",
            alloc::vec![TypeArg::Array {
                element: Box::new(int.clone()),
                rank: 3
            }]
        ),
        instance("N.List`1", alloc::vec![TypeArg::Pointer(Box::new(int.clone()))]),
        instance("N.List`1", alloc::vec![TypeArg::ByRef(Box::new(int.clone()))]),
        instance("N.List`1", alloc::vec![named("N.Foo")]),
        instance("N.List`1", alloc::vec![named("N.Bar")]),
        instance("N.List`1", alloc::vec![TypeArg::Var(0)]),
        instance("N.List`1", alloc::vec![TypeArg::MVar(0)]),
    ];
    let mut hash = 0x811c_9dc5u32;
    for entry in &corpus {
        hash = fnv1a32(hash, entry.name().as_bytes());
        hash = fnv1a32(hash, b"\n");
    }
    for byte in 0x01..=0x20u8 {
        hash = fnv1a32(hash, primitive_name(byte).as_bytes());
        hash = fnv1a32(hash, b"\n");
    }
    hash
}

/// The BCL name a built-in element type spells as, measured from .NET's own `Type.ToString()`
/// rather than recalled. A byte with no built-in name spells as `?<byte>`, which cannot collide
/// with a real type name and is visible in a tag rather than silently equal to another byte's.
fn primitive_name(byte: u8) -> &'static str {
    match byte {
        element::VOID => "System.Void",
        element::BOOLEAN => "System.Boolean",
        element::CHAR => "System.Char",
        element::I1 => "System.SByte",
        element::U1 => "System.Byte",
        element::I2 => "System.Int16",
        element::U2 => "System.UInt16",
        element::I4 => "System.Int32",
        element::U4 => "System.UInt32",
        element::I8 => "System.Int64",
        element::U8 => "System.UInt64",
        element::R4 => "System.Single",
        element::R8 => "System.Double",
        element::STRING => "System.String",
        element::OBJECT => "System.Object",
        element::I => "System.IntPtr",
        element::U => "System.UIntPtr",
        element::TYPEDBYREF => "System.TypedReference",
        _ => "?",
    }
}

/// One instantiation of one generic definition, closed: no type parameter survives anywhere in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instantiation {
    /// The generic definition's full name with its backtick arity (`` System.Collections.Generic.List`1 ``).
    /// This is what a monomorphizer looks up to find the body to substitute into.
    pub definition: Box<str>,
    /// The type arguments, in declaration order.
    pub arguments: Vec<TypeArg>,
    /// Whether the definition is a value type -- the axis the code model turns on (value types
    /// monomorphize, cap 7, and past the cap the tier REFUSES rather than degrading).
    pub value_type: bool,
    /// The canonical spelling. This is the instantiation's identity and the set's key.
    pub name: Box<str>,
    /// The type tag [`exception_tag_for_name`] mints from [`name`](Self::name) -- the same function,
    /// and therefore the same tag space, that every non-generic type's identity already comes from.
    pub tag: u32,
    /// The synthesized [`TypeHandle`] this instantiation is emitted under, from
    /// [`instantiation_handle`]. Cross-assembly stable, because it comes from the name.
    pub handle: TypeHandle,
}

/// Why the collector refused. Every arm is a REFUSAL and none has a fallback: a monomorphizer that
/// silently drops an instantiation emits a body that is never called or, worse, leaves a call
/// pointing at nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The program's static instantiation set is INFINITE. `class C<T> { void M() { new C<C<T>>(); } }`
    /// is legal C# whose closure never terminates, and a monomorphizing tier cannot enumerate it.
    ///
    /// The criterion is GROWTH ON A CYCLE, not recursion: an edge revisiting a definition already
    /// on the current path at STRICTLY GREATER type-argument nesting depth. `class Node<T> { Node<T>
    /// next; }` revisits at equal depth and is finite and perfectly ordinary.
    GrowthOnCycle {
        /// The definition the walk re-entered.
        definition: Box<str>,
        /// The instantiation that re-entered it, spelled.
        name: Box<str>,
        /// The shallowest ARGUMENT nesting this definition already sits at on the path -- 0 for
        /// `C<int>`, whose one argument is a leaf.
        was: u32,
        /// The strictly greater argument nesting that refused it -- 1 for `C<C<int>>`.
        now: u32,
    },
    /// A signature blob did not decode. It is refused rather than skipped for the reason the AOT's
    /// `decodable_params` guard (`lamella_aot::resolver`) exists: a member that silently
    /// ceases to exist is worse than one that loudly fails to load.
    Undecodable {
        /// Where the blob came from, for a message that names something.
        at: Box<str>,
    },
    /// The closure walk exceeded [`PATH_BACKSTOP`]. Not the refusal criterion -- see that constant.
    PathBackstop,
    /// Two DISTINCT instantiations minted the same [`TypeHandle`] from
    /// [`instantiation_handle`]'s 24-bit hash of their names.
    ///
    /// **REFUSED RATHER THAN RISKED, BECAUSE THE FAILURE IS THE ONE THE WHOLE SCHEME EXISTS TO
    /// PREVENT.** Two types under one handle share a DESCRIPTOR: one GC trace map for two layouts,
    /// one tag for two identities. It arrives through the allocator rather than through a cast, but
    /// it is the same hole `generics-identity-and-sharing` s2 forbids. A build that cannot name a
    /// type uniquely must stop, not pick one.
    HandleCollision {
        /// One instantiation's canonical name.
        first: Box<str>,
        /// The other's.
        second: Box<str>,
        /// The handle they both minted.
        handle: u32,
    },
}

/// A program and the assemblies its definitions live in.
///
/// Index 0 is the program: it is where roots come from. Every assembly in the slice, the program
/// included, supplies DEFINITIONS the closure walks into. A definition in an assembly outside the
/// slice is still named -- a `TypeRef` carries its own name, so the spelling never needs the
/// defining assembly -- but its own instantiations are not discovered.
pub struct Program<'a> {
    assemblies: &'a [Assembly<'a>],
    /// Full name -> (assembly index, TypeDef row). Built once; a definition is looked up by NAME
    /// because that is the only identity that crosses an assembly boundary.
    definitions: BTreeMap<Box<str>, (usize, u32)>,
}

impl<'a> Program<'a> {
    /// Indexes `assemblies` by type name. Index 0 is the program; the rest are its references.
    #[must_use]
    pub fn new(assemblies: &'a [Assembly<'a>]) -> Program<'a> {
        let mut definitions = BTreeMap::new();
        for (index, assembly) in assemblies.iter().enumerate() {
            for type_def in assembly.type_defs() {
                if let Some(name) = type_def_full_name(assembly, type_def.token()) {
                    definitions
                        .entry(name.into_boxed_str())
                        .or_insert((index, type_def.token().row()));
                }
            }
        }
        Program {
            assemblies,
            definitions,
        }
    }

    /// Whether a definition named by the set lives in an assembly this `Program` was given, and can
    /// therefore be walked INTO. A caller reports the count of those it cannot, because a closure
    /// that silently stops at an assembly boundary looks exactly like a closure that finished.
    #[must_use]
    pub fn can_walk(&self, definition: &str) -> bool {
        self.definitions.contains_key(definition)
    }

    /// How many methods a definition declares -- the number of BODIES monomorphizing one
    /// instantiation of it would emit. `None` when the definition is not one of ours to see.
    ///
    /// It counts DECLARED methods, not `T`-dependent ones. A measured corpus put `M` at 58.9% of a generic
    /// type's methods as `T`-dependent and reported it as a LOWER BOUND, so the honest reading of a
    /// total built from this is an UPPER bound on bodies that must duplicate -- and both bounds are
    /// compile-time, which is not the count the cap governs.
    #[must_use]
    pub fn definition_method_count(&self, definition: &str) -> Option<usize> {
        let &(index, row) = self.definitions.get(definition)?;
        Some(self.assemblies[index].type_def(row)?.methods().count())
    }

    /// Whether a definition is an INTERFACE, or `None` when it is not one of ours to see.
    ///
    /// It is on the price rather than on the shape: an interface instantiation costs a tag and an
    /// itable entry and NO body, while a class or struct instantiation costs a body per method. A
    /// count that does not separate them prices a program at several times what it pays.
    #[must_use]
    pub fn is_interface(&self, definition: &str) -> Option<bool> {
        let &(index, row) = self.definitions.get(definition)?;
        Some(self.assemblies[index].type_def(row)?.is_interface())
    }

    /// The closed instantiation set, in discovery order, or the refusal that stopped it.
    pub fn instantiations(&self) -> Result<Vec<Instantiation>, Refusal> {
        let mut walk = Walk {
            program: self,
            seen: BTreeSet::new(),
            found: Vec::new(),
        };
        let mut path = Vec::new();
        for root in self.roots()? {
            walk.visit(&root, &mut path)?;
        }
        let mut minted: BTreeMap<u32, Box<str>> = BTreeMap::new();
        for entry in &walk.found {
            if let Some(first) = minted.insert(entry.handle.0, entry.name.clone())
                && first != entry.name
            {
                return Err(Refusal::HandleCollision {
                    first,
                    second: entry.name.clone(),
                    handle: entry.handle.0,
                });
            }
        }
        Ok(walk.found)
    }

    /// Every closed instantiation ANY assembly in the set names directly: its `TypeSpec` rows, the
    /// type arguments of its `MethodSpec` rows, and the field and method signatures of its own
    /// types.
    ///
    /// Open ones are skipped here rather than refused: `List<!0>` inside a generic definition is
    /// not a root, it is an EDGE from that definition's own instantiations, and the closure walk is
    /// what closes it.
    ///
    fn roots(&self) -> Result<Vec<TypeArg>, Refusal> {
        let mut roots = Vec::new();
        for assembly in self.assemblies {
            let tables = assembly.tables();
            for index in 1..=tables.row_count(table::TYPE_SPEC) {
                let token = Token::new(table::TYPE_SPEC, index);
                let ty = self.type_spec(assembly, token)?;
                collect_closed(&ty, &mut roots);
            }
            for index in 1..=tables.row_count(table::METHOD_SPEC) {
                let Some(row) = tables.row(table::METHOD_SPEC, index) else {
                    continue;
                };
                for argument in self.method_spec_arguments(assembly, row.raw(1))? {
                    collect_closed(&argument, &mut roots);
                }
            }
            for type_def in assembly.type_defs() {
                for field in type_def.fields() {
                    let ty = self.field_signature(assembly, field.token())?;
                    collect_closed(&ty, &mut roots);
                }
                for method in type_def.methods() {
                    for ty in self.method_signature(assembly, method.signature_blob())? {
                        collect_closed(&ty, &mut roots);
                    }
                }
            }
            for index in 1..=tables.row_count(table::MEMBER_REF) {
                let Some(member) = assembly.member_ref(index) else {
                    continue;
                };
                if member.is_field() {
                    if let Some(ty) = member.field_type() {
                        collect_closed(&self.from_sig(assembly, &ty)?, &mut roots);
                    }
                } else {
                    for ty in self.method_signature(assembly, member.signature_blob())? {
                        collect_closed(&ty, &mut roots);
                    }
                }
            }
            for index in 1..=tables.row_count(table::STAND_ALONE_SIG) {
                let token = Token::new(table::STAND_ALONE_SIG, index);
                for ty in self.local_var_types(assembly, token)? {
                    collect_closed(&ty, &mut roots);
                }
            }
        }
        Ok(roots)
    }

    /// The decoded type a `TypeSpec` row stands for.
    fn type_spec(&self, assembly: &Assembly<'a>, token: Token) -> Result<TypeArg, Refusal> {
        let sig = assembly
            .type_spec_signature(token)
            .ok_or_else(|| undecodable("TypeSpec"))?;
        self.from_sig(assembly, &sig)
    }

    /// The decoded type of a `Field` row.
    fn field_signature(&self, assembly: &Assembly<'a>, token: Token) -> Result<TypeArg, Refusal> {
        let sig = assembly
            .field_signature(token)
            .ok_or_else(|| undecodable("Field signature"))?;
        self.from_sig(assembly, &sig)
    }

    /// Every type a METHOD signature mentions: its return type, then its parameters.
    fn method_signature(
        &self,
        assembly: &Assembly<'a>,
        blob: &[u8],
    ) -> Result<Vec<TypeArg>, Refusal> {
        if blob.is_empty() {
            return Ok(Vec::new());
        }
        let sig = parse_method(blob).map_err(|_| undecodable("MethodDef signature"))?;
        let mut types = alloc::vec![self.from_sig(assembly, &sig.return_type)?];
        for parameter in &sig.parameters {
            types.push(self.from_sig(assembly, parameter)?);
        }
        Ok(types)
    }

    /// A `MethodSpec`'s type arguments (II.23.2.15).
    ///
    /// This module walked the blob itself for a few hours because `lamella-metadata` exposed no
    /// decoder for this one shape, finding each argument's end by the shortest prefix `parse_type`
    /// accepted. `parse_method_spec` landed in `5eeb2dd961` and retires that: **one decoder owns
    /// the format, with no shape left over.**
    fn method_spec_arguments(
        &self,
        assembly: &Assembly<'a>,
        blob_index: u32,
    ) -> Result<Vec<TypeArg>, Refusal> {
        let blob = assembly
            .image()
            .blob()
            .get(blob_index)
            .map_err(|_| undecodable("MethodSpec"))?;
        let decoded = parse_method_spec(blob).map_err(|_| undecodable("MethodSpec"))?;
        let mut arguments = Vec::new();
        for argument in &decoded {
            arguments.push(self.from_sig(assembly, argument)?);
        }
        Ok(arguments)
    }

    /// The local-variable types a `StandAloneSig` row declares (II.23.2.6). A generic instantiation
    /// hides here as readily as in a field: `List<T> local` is a local, not a signature.
    fn local_var_types(
        &self,
        assembly: &Assembly<'a>,
        token: Token,
    ) -> Result<Vec<TypeArg>, Refusal> {
        let Some(blob) = assembly
            .tables()
            .row(table::STAND_ALONE_SIG, token.row())
            .and_then(|row| assembly.image().blob().get(row.raw(0)).ok())
        else {
            return Ok(Vec::new());
        };
        let Ok(locals) = parse_local_vars(blob) else {
            return Ok(Vec::new());
        };
        let mut types = Vec::new();
        for local in &locals {
            types.push(self.from_sig(assembly, &local.ty)?);
        }
        Ok(types)
    }

    /// Converts a decoded [`SigType`] into a [`TypeArg`] -- [`sig_to_type_arg`], which needs only
    /// the one assembly the tokens belong to.
    fn from_sig(&self, assembly: &Assembly<'a>, ty: &SigType) -> Result<TypeArg, Refusal> {
        sig_to_type_arg(assembly, ty)
    }

}

/// Appends every CLOSED instantiation inside `ty` -- itself included when it is one -- to `out`.
/// An open one contributes nothing: it is an edge, not a root.
fn collect_closed(ty: &TypeArg, out: &mut Vec<TypeArg>) {
    match ty {
        TypeArg::Instance { arguments, .. } => {
            if ty.is_closed() {
                out.push(ty.clone());
            }
            for argument in arguments {
                collect_closed(argument, out);
            }
        }
        TypeArg::SzArray(inner)
        | TypeArg::Array { element: inner, .. }
        | TypeArg::Pointer(inner)
        | TypeArg::ByRef(inner) => collect_closed(inner, out),
        TypeArg::Primitive(_) | TypeArg::Named { .. } | TypeArg::Var(_) | TypeArg::MVar(_) => {}
    }
}

fn undecodable(at: &str) -> Refusal {
    Refusal::Undecodable {
        at: at.to_owned().into_boxed_str(),
    }
}

/// The closure walk's state.
struct Walk<'p, 'a> {
    program: &'p Program<'a>,
    /// Canonical names already expanded. Dedup is by NAME because the name is the identity.
    seen: BTreeSet<Box<str>>,
    found: Vec<Instantiation>,
}

impl Walk<'_, '_> {
    /// Adds `ty` to the set if it is a closed instantiation, then walks every instantiation its
    /// definition reaches under the same substitution.
    fn visit(&mut self, ty: &TypeArg, path: &mut Vec<(Box<str>, u32)>) -> Result<(), Refusal> {
        let TypeArg::Instance {
            definition,
            value_type,
            arguments,
        } = ty
        else {
            return Ok(());
        };
        if !ty.is_closed() {
            return Ok(());
        }
        let name = ty.name().into_boxed_str();
        if self.seen.contains(&name) {
            return Ok(());
        }
        let depth = arguments.iter().map(TypeArg::depth).max().unwrap_or(0);
        if let Some(was) = path
            .iter()
            .filter(|(on_path, _)| on_path == definition)
            .map(|(_, depth)| *depth)
            .min()
            && depth > was
        {
            return Err(Refusal::GrowthOnCycle {
                definition: definition.clone(),
                name,
                was,
                now: depth,
            });
        }
        if path.len() >= PATH_BACKSTOP {
            return Err(Refusal::PathBackstop);
        }
        self.seen.insert(name.clone());
        self.found.push(Instantiation {
            definition: definition.clone(),
            arguments: arguments.clone(),
            value_type: *value_type,
            tag: exception_tag_for_name("", &name),
            handle: instantiation_handle(&name),
            name,
        });
        path.push((definition.clone(), depth));
        let outcome = self.expand(definition, arguments, path);
        path.pop();
        outcome
    }

    /// Every instantiation reachable from `definition` once its type parameters are `arguments`:
    /// its base type and interfaces, its fields, its methods' signatures and locals, and the
    /// `TypeSpec` / `MethodSpec` tokens its method bodies name.
    fn expand(
        &mut self,
        definition: &str,
        arguments: &[TypeArg],
        path: &mut Vec<(Box<str>, u32)>,
    ) -> Result<(), Refusal> {
        let Some(&(index, row)) = self.program.definitions.get(definition) else {
            return Ok(());
        };
        let assembly = &self.program.assemblies[index];
        let Some(type_def) = assembly.type_def(row) else {
            return Ok(());
        };
        let mut edges = Vec::new();
        for token in
            core::iter::once(type_def.extends()).chain(type_def.interfaces().collect::<Vec<_>>())
        {
            if token.table() == table::TYPE_SPEC {
                edges.push(self.program.type_spec(assembly, token)?);
            }
        }
        for field in type_def.fields() {
            edges.push(self.program.field_signature(assembly, field.token())?);
        }
        for method in type_def.methods() {
            edges.extend(
                self.program
                    .method_signature(assembly, method.signature_blob())?,
            );
            let Some(body) = method.body() else {
                continue;
            };
            if let Some(local_sig) = body.local_var_sig {
                edges.extend(self.program.local_var_types(assembly, local_sig)?);
            }
            for instruction in body.code.iter() {
                let Operand::Token(token) = &instruction.operand else {
                    continue;
                };
                match token.table() {
                    table::TYPE_SPEC => edges.push(self.program.type_spec(assembly, *token)?),
                    table::METHOD_SPEC => {
                        let Some(spec) = assembly.tables().row(table::METHOD_SPEC, token.row())
                        else {
                            continue;
                        };
                        edges.extend(self.program.method_spec_arguments(assembly, spec.raw(1))?);
                        let parent = CodedIndex::MethodDefOrRef.decode(spec.raw(0));
                        if parent.table() == table::MEMBER_REF {
                            edges.extend(self.member_ref_parent(assembly, parent)?);
                        }
                    }
                    table::MEMBER_REF => edges.extend(self.member_ref_parent(assembly, *token)?),
                    _ => {}
                }
            }
        }
        for edge in &edges {
            let Some(closed) = edge.substitute(arguments, &[]) else {
                continue;
            };
            let mut nested = Vec::new();
            collect_closed(&closed, &mut nested);
            for instantiation in &nested {
                self.visit(instantiation, path)?;
            }
        }
        Ok(())
    }

    /// The declaring type of a `MemberRef`, when that type is an instantiation.
    fn member_ref_parent(
        &self,
        assembly: &Assembly<'_>,
        token: Token,
    ) -> Result<Vec<TypeArg>, Refusal> {
        let Some(row) = assembly.tables().row(table::MEMBER_REF, token.row()) else {
            return Ok(Vec::new());
        };
        let parent = row.token(0);
        if parent.table() != table::TYPE_SPEC {
            return Ok(Vec::new());
        }
        Ok(alloc::vec![self.program.type_spec(assembly, parent)?])
    }
}

/// One (generic method, call-site type arguments) pair -- the unit a tier emits a body for.
///
/// It names the DECLARING TYPE rather than carrying a metadata row, because a row is an index into
/// one assembly's tables and this set crosses assemblies. A name is the identity both tiers already
/// share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodPair {
    /// The full name of the type declaring the body this pair runs.
    pub declaring: Box<str>,
    /// The method's name.
    pub method: Box<str>,
    /// Its declared parameter signature, still open where it mentions `!n` / `!!n`.
    pub parameters: Vec<SigType>,
    /// How many type parameters the method itself declares.
    pub arity: u32,
    /// The type arguments the CALL SITE supplied.
    pub arguments: Vec<SigType>,
}

impl Program<'_> {
    /// Closes `named` over the OVERRIDE relation: for every pair the caller found, every override of
    /// that method in the program's closed hierarchy, at the SAME type arguments.
    ///
    /// # The assumption this exists to break
    ///
    /// A consumer of the `MethodSpec` table naturally reads it as *a finite, already-closed
    /// enumeration of the generic methods a program calls*. **That is true for static and
    /// non-virtual generic methods and FALSE for virtual ones.** `b.Tag<int>()` on a `Base b` emits
    /// ONE row, naming `Base::Tag`; the body that must actually run is `Derived::Tag<int>`, and
    /// **nothing anywhere names it**. Lowering only the pair the token names BINDS -- to the base's
    /// declaration -- so the program loads, bakes clean, runs, and answers wrong. That is why the
    /// set has to be closed rather than read.
    ///
    /// # Why it terminates
    ///
    /// The arguments come from the table and are never grown here: an override is emitted at the
    /// SAME arguments as the pair it overrides, so this adds bodies and never new instantiations.
    /// The hierarchy is closed at bake time and finite. There is no growth-on-a-cycle refusal to
    /// make, unlike [`Program::instantiations`], because nothing in this walk can enlarge an
    /// argument list.
    ///
    /// # What it matches on, and why not a string
    ///
    /// Name, generic ARITY and the declared parameter signature, compared STRUCTURALLY. ECMA-335
    /// II.9.9 makes arity part of the rule outright (*"the number of generic parameters shall match
    /// exactly those of the overridden method"*), and a structural comparison is the one form of
    /// identity that cannot drift from someone else's spelling rule -- the same reason
    /// `find_instantiation` compares decoded arguments rather than names.
    ///
    /// # What it cannot see, and says so by omission
    ///
    /// A definition in an assembly this `Program` was not given is not walked, so an override
    /// declared there is not found. That is [`Program::can_walk`]'s stated boundary rather than a
    /// silent skip, and a caller that needs the guarantee must ask it.
    ///
    /// **AN EXPLICIT INTERFACE IMPLEMENTATION IS NOT COVERED.** `int IFoo.Tag<T>()` is named through
    /// the `MethodImpl` table under a MANGLED name, so it is a third LOOKUP rather than a third
    /// branch, and neither the class rule nor the interface rule below finds it. An IMPLICIT
    /// implementation is covered. A caller must not read this function's answer as covering both.
    #[must_use]
    pub fn close_over_overrides(&self, named: &[MethodPair]) -> Vec<MethodPair> {
        let mut found: Vec<MethodPair> = Vec::new();
        let mut seen: BTreeSet<(Box<str>, Box<str>, u32)> = BTreeSet::new();
        for pair in named {
            let declaring_is_interface = self.is_interface(&pair.declaring).unwrap_or(false);
            for (candidate, &(index, row)) in &self.definitions {
                if candidate.as_ref() == pair.declaring.as_ref() {
                    continue;
                }
                let related = if declaring_is_interface {
                    self.implements(candidate, &pair.declaring)
                } else {
                    self.derives_from(candidate, &pair.declaring)
                };
                if !related {
                    continue;
                }
                let assembly = &self.assemblies[index];
                let Some(type_def) = assembly.type_def(row) else {
                    continue;
                };
                for method in type_def.methods() {
                    if !method.is_virtual() {
                        continue;
                    }
                    if !declaring_is_interface && method.flags() & METHOD_NEWSLOT != 0 {
                        continue;
                    }
                    if method.name() != Some(pair.method.as_ref()) {
                        continue;
                    }
                    let Some(signature) = method.signature() else {
                        continue;
                    };
                    if signature.generic_param_count != pair.arity
                        || signature.parameters != pair.parameters
                    {
                        continue;
                    }
                    let key = (
                        candidate.clone(),
                        pair.method.clone(),
                        pair.arity,
                    );
                    if !seen.insert(key) {
                        continue;
                    }
                    found.push(MethodPair {
                        declaring: candidate.clone(),
                        method: pair.method.clone(),
                        parameters: signature.parameters.clone(),
                        arity: pair.arity,
                        arguments: pair.arguments.clone(),
                    });
                }
            }
        }
        found
    }

    /// Whether `candidate` implements `interface`, by NAME.
    ///
    /// Both edges are followed, and each is load-bearing: a type inherits its BASE's interface list
    /// (`class D : C` where `C : IFoo` implements `IFoo`), and an interface can EXTEND another
    /// (`IDerived : IBase` means an implementer of `IDerived` also implements `IBase`). Following
    /// only the type's own `interfaces()` row answers the one-line case and misses both.
    ///
    /// Breadth-first over a worklist with a visited set, rather than the depth-bounded chain
    /// [`Program::derives_from`] uses: interfaces form a DAG, not a chain, so the same interface is
    /// reachable by several routes and a depth bound alone would either stop early or revisit.
    fn implements(&self, candidate: &str, interface: &str) -> bool {
        let mut seen: BTreeSet<Box<str>> = BTreeSet::new();
        let mut queue: Vec<Box<str>> = alloc::vec![candidate.into()];
        while let Some(at) = queue.pop() {
            if !seen.insert(at.clone()) {
                continue;
            }
            if seen.len() > PATH_BACKSTOP {
                return false;
            }
            let Some(&(index, row)) = self.definitions.get(at.as_ref()) else {
                continue;
            };
            let assembly = &self.assemblies[index];
            let Some(type_def) = assembly.type_def(row) else {
                continue;
            };
            for token in
                core::iter::once(type_def.extends()).chain(type_def.interfaces().collect::<Vec<_>>())
            {
                if token.0 == 0 {
                    continue;
                }
                let name = if token.table() == table::TYPE_SPEC {
                    instantiation_of(assembly, token).map(|(name, _)| name)
                } else {
                    type_def_full_name(assembly, token)
                };
                let Some(name) = name else {
                    continue;
                };
                if name == interface {
                    return true;
                }
                queue.push(name.into_boxed_str());
            }
        }
        false
    }

    /// Whether `candidate` reaches `ancestor` by following `extends`, by NAME.
    ///
    /// Bounded by [`PATH_BACKSTOP`] rather than by a visited set: a base chain is not a graph and a
    /// cycle in one is malformed metadata, so the honest response is to stop walking rather than to
    /// tidy it into an answer.
    fn derives_from(&self, candidate: &str, ancestor: &str) -> bool {
        let mut at = candidate;
        let mut owned;
        for _ in 0..PATH_BACKSTOP {
            let Some(&(index, row)) = self.definitions.get(at) else {
                return false;
            };
            let assembly = &self.assemblies[index];
            let Some(type_def) = assembly.type_def(row) else {
                return false;
            };
            let extends = type_def.extends();
            if extends.0 == 0 {
                return false;
            }
            let name = if extends.table() == table::TYPE_SPEC {
                instantiation_of(assembly, extends).map(|(name, _)| name)
            } else {
                type_def_full_name(assembly, extends)
            };
            let Some(name) = name else {
                return false;
            };
            if name == ancestor {
                return true;
            }
            owned = name;
            at = &owned;
        }
        false
    }
}

/// `mdNewSlot` (II.23.1.10) -- the method starts a new vtable slot rather than overriding one.
const METHOD_NEWSLOT: u32 = 0x0100;

/// A type token's full name: the enclosing chain joined with `+`, prefixed by the OUTERMOST type's
/// namespace, exactly as .NET spells a nested type.
///
/// It works for a `TypeRef` as well as a `TypeDef`, and that is load-bearing: a reference carries
/// its own namespace and name, so the spelling never needs the defining assembly to be present.
///
/// **PUBLIC BECAUSE A CONSUMER OF THE PLAN HAS TO ASK THE SAME QUESTION AND MUST NOT RE-SPELL IT.**
/// [`MonoMethodBody::declaring`] is written by this function; a dispatch table looking a body up by
/// its declaring type computes the key with it too, so the two sides cannot drift. A second
/// spelling of a nested chain is exactly where they would.
#[must_use]
pub fn type_def_full_name(assembly: &Assembly<'_>, token: Token) -> Option<String> {
    let mut chain = Vec::new();
    let mut namespace;
    let mut current = token;
    loop {
        match current.table() {
            table::TYPE_DEF => {
                let type_def = assembly.type_def(current.row())?;
                let name = type_def.name()?;
                chain.push(name.name);
                namespace = name.namespace;
                match type_def.enclosing_type() {
                    Some(enclosing) => current = enclosing.token(),
                    None => break,
                }
            }
            table::TYPE_REF => {
                let type_ref = assembly.type_ref(current.row())?;
                let name = type_ref.name()?;
                chain.push(name.name);
                namespace = name.namespace;
                let scope = type_ref.resolution_scope();
                if scope.table() == table::TYPE_REF {
                    current = scope;
                } else {
                    break;
                }
            }
            _ => return None,
        }
        if chain.len() > PATH_BACKSTOP {
            return None;
        }
    }
    chain.reverse();
    let mut out = String::new();
    if !namespace.is_empty() {
        out.push_str(namespace);
        out.push('.');
    }
    for (index, part) in chain.iter().enumerate() {
        if index > 0 {
            out.push('+');
        }
        out.push_str(part);
    }
    Some(out)
}

/// One MONOMORPHIZED BODY a build emits: which definition's CIL supplies it, which instantiation it
/// is lowered under, and the function index it occupies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonoBody {
    /// The function index this body occupies -- past `max_rid`, in the SAME index space
    /// `Inst::Call { callee }` already names.
    ///
    /// **IT IS NOT A MethodDef RID AND THERE IS DELIBERATELY NO SYNTHETIC ONE.** The reachability
    /// walk's worklist is a `Vec<u32>` of FUNCTION INDICES; it calls its variable `rid` only because
    /// nothing has ever occupied an index above `max_rid`. So the index space EXTENDS and the
    /// boundary is DERIVED (`max_rid` is computed from the metadata) rather than maintained, which is
    /// the whole reason a synthetic rid -- a hand-maintained twin of a metadata row -- is not needed.
    pub index: u32,
    /// The instantiation's canonical spelling -- the identity half of the call-site key, and the
    /// only identity that survives an assembly boundary.
    pub instantiation: Box<str>,
    /// The definition's full name, carried beside [`Self::arguments`] and `None` on the same terms.
    /// Kept rather than split back out of the instantiation's spelling, because this crate has one
    /// speller and re-deriving a definition from a spelled name would be a second reading of it.
    pub definition: Option<Box<str>>,
    /// The CLOSED type arguments, carried rather than derived, for an instantiation the module
    /// spells through NO `TypeSpec` row of its own.
    ///
    /// **`None` IS THE ORDINARY CASE AND KEEPS EVERY EXISTING BODY BYTE-IDENTICAL:** the module
    /// named the instantiation, [`Self::spec`] is its row, and the arguments are decoded from it as
    /// they always were. `Some` is a body the CLOSURE found -- `ListEnumerator<int>` reached only
    /// through `List<int>.GetEnumerator`'s own CIL -- for which no row exists to decode, so the
    /// arguments the walk resolved travel with the body instead.
    pub arguments: Option<Vec<SigType>>,
    /// The `TypeSpec` token, in the module's OWN assembly, that spells this instantiation.
    ///
    /// **The token is kept rather than the arguments' names because the ARGUMENTS are what
    /// substitution needs, and they are `SigType`s whose `Class`/`ValueType` tokens are meaningful
    /// only in this assembly.** [`substitute_sig`]'s own documentation records why the name-keyed
    /// [`TypeArg`] cannot be converted back: doing so would have to invent tokens.
    pub spec: Token,
    /// The `MethodDef` rid of the definition method whose CIL body is lowered under the
    /// instantiation.
    ///
    /// **SEVERAL BODIES SHARE ONE RID -- that is what monomorphization IS**, and it is exactly why
    /// they cannot be rid-indexed: a second body written to one rid REPLACES the first, silently,
    /// which is what `lamella_aot::build::BuildError::DuplicateMethodBody` refuses.
    pub rid: u32,
    /// The method's name.
    pub name: Box<str>,
    /// The definition's declared parameters, still spelled with `!n` -- the OVERLOAD half of the
    /// call-site key.
    ///
    /// A `MemberRef` whose parent is a `TypeSpec` carries the DEFINITION's signature verbatim
    /// (ECMA-335 II.22.25), so a call site's parameters and these compare directly with no
    /// substitution on either side. Matching on the NAME alone would bind an overload to its
    /// sibling, which is the fabricated-nullary collision one more layer out.
    pub parameters: Vec<SigType>,
    /// WHICH ASSEMBLY'S METADATA `rid` INDEXES. See [`BodyOwner`].
    pub owner: BodyOwner,
    /// Whether this entry stands for a declaration with NO CIL -- an ABSTRACT method of the
    /// definition -- rather than a body to substitute into. The emitter lays a TRAP at its index.
    ///
    /// **A DISPATCH TABLE NEEDS A SLOT FOR A DECLARATION THAT HAS NO BODY, and the slot cannot be
    /// omitted.** A vtable with a slot left out is not a smaller vtable: every slot after it shifts,
    /// so a `callvirt` computed against the numbering lands on a different method. The plan is where
    /// the index comes from, so a declaration with no body still needs an entry here -- otherwise
    /// `instantiation_dispatch` has nothing to name and refuses the whole instantiation, which is
    /// what stood between this tier and any abstract generic (`EqualityComparer<T>` among them).
    ///
    /// **REACHING IT IS IMPOSSIBLE RATHER THAN MERELY UNLIKELY**, which is why a trap is honest and
    /// not defensive: no instance of an abstract type exists, and a `callvirt` on a real object
    /// dispatches through THAT object's descriptor -- the concrete derived type's, whose slot holds
    /// the override. This type's descriptor is read for type tests and as a declared type, neither
    /// of which touches the vtable. A trap rather than a returning stub because an unreachable slot
    /// that answers a value is absence rendered as a confident value, which is the shape this
    /// project has now been bitten by four times.
    pub declaration_only: bool,
}

/// Which assembly declares the definition whose CIL a [`MonoBody`] lowers.
///
/// # Why this is carried rather than re-derived
///
/// **THE COLLECTOR ALREADY KNOWS, AND ASKING AGAIN IS THIS LANE'S RECURRING BUG CLASS.** Deciding
/// "which assembly is this rid in" a second time, at the emitter, means two answers that agree
/// until one of them meets a case the other does not -- and the disagreement is silent, because
/// `assembly.method(rid)` returns a perfectly good method from the WRONG assembly. So the owner is
/// decided once, where the name was resolved, and handed out.
///
/// **AN ORDINAL IS ONLY MEANINGFUL AGAINST THE REFERENCE LIST THE PLAN WAS BUILT WITH.** It is
/// the same ordinal a descriptor symbol, a statics region and a reference-owned `TypeHandle` all
/// encode, and they are identity -- so a plan built against one reference order and consumed
/// against another is a wrong bind, not a lookup failure. Build and consume with one list.
/// **DELIBERATELY NOT `Default`.** The safe-looking default is `Own`, which is exactly the value
/// a forgotten assignment would take and exactly the one that reads a rid out of the wrong
/// assembly without complaint. Constructing a body must state where its CIL lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyOwner {
    /// The assembly being built declares the definition: `rid` is its own `MethodDef` row.
    Own,
    /// A REFERENCED assembly declares it, at this ordinal in the reference list the plan was built
    /// with; `rid` is a `MethodDef` row of THAT assembly.
    Reference(u8),
}

/// One monomorphized body of a generic METHOD: the definition's CIL lowered under the type
/// arguments a single call site supplied.
///
/// # Why this is not a [`MonoBody`] with an extra field
///
/// **THE TWO AXES ARE KEYED DIFFERENTLY, AND THAT IS THE ONLY REASON THE TYPE AXIS COULD NOT BE
/// REUSED.** A type instantiation is found by `(spelling, method name, parameters)`, because a call
/// site names it through a `MemberRef` parented by a `TypeSpec` and the overload has to be told from
/// its siblings. A generic METHOD's call site names a `MethodSpec` row, and that row IS the
/// `(method, arguments)` pair -- so the token is the key, exactly, with no spelling comparison and
/// no overload question. Folding both into one struct would mean one field meaning two things
/// depending on a second field, which is how a wrong bind gets written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonoMethodBody {
    /// The function index this body occupies -- past `max_rid`, in the same space
    /// [`MonoBody::index`] uses and `Inst::Call { callee }` already names.
    pub index: u32,
    /// The `MethodDef` rid of the generic definition whose CIL is lowered under `arguments`.
    ///
    /// **SEVERAL BODIES SHARE ONE RID**, exactly as on the type axis -- `Pick<int>` and
    /// `Pick<string>` are one row and two bodies.
    pub rid: u32,
    /// The method's name.
    pub name: Box<str>,
    /// The call site's type arguments, closed, in `!!n` order -- what `!!0` substitutes to.
    pub arguments: Vec<SigType>,
    /// The full name of the type DECLARING this body.
    ///
    /// **A rid alone cannot say which type's dispatch this body belongs to, and the override
    /// closure makes that a live question rather than a tidy one.** `Base::Tag<int>` and
    /// `Derived::Tag<int>` are two bodies of one `(name, arity, parameters, arguments)` pair, told
    /// apart only by this -- so a consumer placing them in vtable slots reads it, and
    /// `dump-mono-bodies` prints it, because "which `MethodDef` did this bind to" is unanswerable
    /// from a rid the reader would have to look up.
    pub declaring: Box<str>,
    /// The canonical spelling of the pair (`` Pick``1[System.Int32] ``), for diagnostics and for the
    /// symbol an eventual cross-assembly identity would need. Not the lookup key: the token is.
    pub instantiation: Box<str>,
    /// WHICH ASSEMBLY'S METADATA `rid` INDEXES -- the same field [`MonoBody`] carries, for the same
    /// reason. See [`BodyOwner`].
    pub owner: BodyOwner,
}

/// Every monomorphized body a module emits, and the map from a CALL SITE to the index its body
/// occupies.
///
/// # THE ORDERING IS FORCED, AND THIS TYPE IS WHAT MAKES IT EXPRESSIBLE
///
/// A reachability walk discovers a monomorphized body **the ordinary way, through `Inst::Call`** --
/// which it can only do if the resolver already answered a call with that body's index. So the plan
/// is built from METADATA ALONE, before anything is lowered: **collect, assign indices, THEN walk.**
/// On the RISC-V object path a body the walk never reached is a `stub()` that RETURNS -- a silent
/// wrong answer rather than a link error -- so getting this order wrong is not a build failure.
///
/// # What it deliberately does not cover yet
///
/// **A definition in a REFERENCED assembly is planned only when the references are supplied**
/// ([`for_assembly_with_references`](Self::for_assembly_with_references)), and **no shipping build
/// path supplies them**, so on every path that ships today `List<int>` is absent while `Box<int>`
/// is present. Measured on a two-assembly fixture: the collector finds both
/// `` Box`1[System.Int32] `` and `` Box`1[System.String] ``, the plan emits ZERO bodies, and
/// `Main` refuses with `UnresolvedCall`. It never falls back to the definition's own body, which
/// would be one body serving every instantiation.
///
/// **AND THE MISSING HALF IS EMISSION, NOT COLLECTION.** A planned reference-owned body carries
/// a rid into the OWNER's tables and CIL full of the owner's tokens. Until that rebase exists, a
/// consumer must read [`MonoBody::owner`] and decline rather than lower -- lowering it against the
/// caller reads a real method from the wrong assembly and produces a plausible wrong answer.
///
/// **AND THE OBSTRUCTION IS NARROWER THAN "TWO TOKEN SPACES", WHICH IS WHAT MEASURING IT
/// CORRECTED.** A [`MonoBody`] carries a `TypeSpec` token from the CALLER, and lowering a
/// reference's body needs a resolver over the REFERENCE, in whose token space that token means
/// nothing -- so the carrier, not the idea, is what is program-local. But a type ARGUMENT is only
/// token-bearing when it is NAMED: `SigType::I4` and `SigType::String` carry no token at all, so
/// `List<int>` could be planned by carrying its arguments as `SigType`s directly, while
/// `List<MyProgramClass>` genuinely cannot without a name-keyed argument. **The cheap slice and the
/// hard one are different problems, and calling both "the cross-assembly case" hides that.**
#[derive(Debug, Clone, Default)]
pub struct MonoPlan {
    bodies: Vec<MonoBody>,
    method_bodies: Vec<MonoMethodBody>,
    /// Every `MethodSpec` token this assembly declares that the plan LOWERS, paired with the index
    /// of the body it binds to. Several tokens may share one index -- two rows naming the same
    /// `(method, arguments)` pair are one body -- so this is a map and not a parallel array.
    ///
    /// A token ABSENT here is one the plan does not lower, and its call site then finds no binding
    /// at all and refuses `UnresolvedCall`. **That absence is load-bearing and is why nothing here
    /// falls back to the definition's own body**: binding a `MethodSpec` to the `MethodDef` it names
    /// produces a program that links, runs, and answers with `!!0` never substituted.
    ///
    /// **A VIRTUAL PAIR IS NOT IN HERE.** See [`Self::virtual_spec_index`].
    method_spec_index: Vec<(Token, u32)>,
    /// The same map for a `MethodSpec` naming a VIRTUAL generic method: the token, and the index of
    /// the body ITS OWN `MethodDef` contributes -- the base's declaration, which is the right answer
    /// for a base receiver and the WRONG one for every derived receiver.
    ///
    /// **IT IS A SECOND FIELD RATHER THAN A FLAG, AND THAT IS THE WHOLE SAFETY PROPERTY.** A caller
    /// reading [`Self::method_index_of`] gets exactly what it always got: a body it may bind
    /// directly. Reaching a virtual pair takes [`Self::virtual_method_index_of`], which cannot be
    /// arrived at by accident and whose every caller must have decided what to do about dispatch.
    /// Folding the two into one map with a boolean beside it would make the safe read the DEFAULT
    /// read, and a caller that forgot to consult the boolean would emit a direct call to the base --
    /// a program that builds, links, runs and answers 5 where 42 is correct.
    virtual_spec_index: Vec<(Token, u32)>,
    /// Every distinct VIRTUAL generic pair the module's `MethodSpec` table spells, in table order.
    ///
    /// **A DISPATCH TABLE'S SLOTS ARE A PROGRAM-GLOBAL QUESTION AND THIS IS WHERE IT IS ANSWERED.**
    /// Which argument lists a virtual generic method expands into must be the same for EVERY type in
    /// a hierarchy: a `callvirt` through a `Base`-typed receiver computes its slot in `Base`'s
    /// numbering and indexes `Derived`'s table, so a per-type answer -- "the arguments THIS type has
    /// bodies for" -- would number the two differently the moment one of them failed to override.
    virtual_pairs: Vec<MethodPair>,
}

/// What [`MonoPlan::method_axis`] produces: the generic-METHOD half of a plan, before it is joined
/// with the type half.
///
/// It is a named struct rather than a tuple because the two `(Token, u32)` maps in it are the two
/// halves of the virtual-generic guard, and they are the same TYPE -- so a tuple return puts the
/// safe map and the unsafe one one position apart with nothing to catch a swap.
struct MethodAxis {
    method_bodies: Vec<MonoMethodBody>,
    /// `MethodSpec` -> body, for pairs a caller may bind DIRECTLY.
    index: Vec<(Token, u32)>,
    /// `MethodSpec` -> the base's body, for pairs that must DISPATCH.
    virtual_index: Vec<(Token, u32)>,
    virtual_pairs: Vec<MethodPair>,
}

impl MonoPlan {
    /// The plan for `assembly`'s own instantiations, numbering bodies from `first_index`.
    ///
    /// `first_index` is `max_rid + 1` for the module being built: every rid up to `max_rid` is a
    /// method's slot, so the first free index is the one after it.
    ///
    /// Every instantiation the assembly SPELLS (a `TypeSpec` row) that is closed and whose definition
    /// this assembly declares contributes one body per method the definition declares WITH A BODY --
    /// an abstract or extern method has no CIL to substitute into and no body to emit.
    ///
    /// **An UNDECODABLE method signature REFUSES the whole plan rather than skipping the method.**
    /// A skipped method cannot be keyed, so a call to it would bind to a same-named sibling -- the
    /// wrong-bind shape, not a missing-bind one. It is the same rule `decodable_params` follows.
    pub fn for_assembly(assembly: &Assembly<'_>, first_index: u32) -> Result<MonoPlan, Refusal> {
        Self::for_assembly_with_references(assembly, &[], first_index)
    }

    /// [`for_assembly`](Self::for_assembly), with the REFERENCES available so an instantiation of a
    /// definition declared next door can be planned too.
    ///
    /// **AN EMPTY REFERENCE LIST REPRODUCES `for_assembly` EXACTLY, AND THAT IS THE SAFETY
    /// PROPERTY.** A `TypeRef` definition cannot resolve against no references, so every
    /// instantiation whose definition is imported is declined by the same path as before -- which
    /// is why the existing callers can keep calling `for_assembly` and emit byte-identical images
    /// while this grows underneath them.
    ///
    /// **PLANNING A CROSS-ASSEMBLY BODY IS NOT THE SAME AS BEING ABLE TO EMIT ONE.** The
    /// definition's CIL is in the OWNER's token space: every field, call and string literal in it
    /// is an owner token, while the emitted body lands in the CALLER's function table. A consumer
    /// that lowers one of these bodies with a resolver over the CALLER produces a body that links,
    /// boots and answers wrong. [`MonoBody::owner`] is what a consumer must read before it lowers,
    /// Plans the instantiations the CLOSURE finds that this module's own `TypeSpec` table does not
    /// name -- the transitive half of the type axis.
    ///
    /// The arguments are carried on the body rather than decoded from a row, because there is no row:
    /// the walk resolves them by NAME and [`type_arg_to_sig`] turns that back into a signature the
    /// module could have written, by looking each name up in the module's OWN tables.
    ///
    /// **AN INSTANTIATION WHOSE ARGUMENTS CANNOT BE EXPRESSED IS SKIPPED, NOT GUESSED AT.** The call
    /// site then stays exactly as refused as it is today, which is the direction that cannot put a
    /// wrong type in an image.
    /// A full dotted name split the way [`Assembly::find_type`] wants it -- everything before the
    /// last `.` is the namespace, the remainder is the simple name, and a name with no `.` is in the
    /// global namespace. The inverse of the join `type_def_full_name` performs.
    fn split_full_name(full: &str) -> (&str, &str) {
        full.rsplit_once('.').unwrap_or(("", full))
    }

    fn extend_with_closure<'a>(
        assembly: &Assembly<'a>,
        references: &[&Assembly<'a>],
        bodies: &mut Vec<MonoBody>,
        seen: &mut BTreeSet<Box<str>>,
        next: &mut u32,
    ) -> Result<(), Refusal> {
        let mut assemblies: Vec<Assembly<'a>> = Vec::with_capacity(1 + references.len());
        assemblies.push(*assembly);
        assemblies.extend(references.iter().map(|reference| **reference));
        let walk = Program::new(&assemblies);
        let Ok(closed) = walk.instantiations() else {
            return Ok(());
        };
        let mut tokens: BTreeMap<String, Token> = BTreeMap::new();
        for type_ref in assembly.type_refs() {
            if let Some(name) = type_def_full_name(assembly, type_ref.token()) {
                tokens.entry(name).or_insert(type_ref.token());
            }
        }
        for type_def in assembly.type_defs() {
            if let Some(name) = type_def_full_name(assembly, type_def.token()) {
                tokens.insert(name, type_def.token());
            }
        }
        for instantiation in closed {
            if seen.contains(&instantiation.name) {
                continue;
            }
            let Some(arguments) = instantiation
                .arguments
                .iter()
                .map(|argument| type_arg_to_sig(argument, &tokens))
                .collect::<Option<Vec<SigType>>>()
            else {
                continue;
            };
            let (namespace, simple) = Self::split_full_name(&instantiation.definition);
            let (owner, type_def) = if let Some(type_def) = assembly.find_type(namespace, simple) {
                (BodyOwner::Own, type_def)
            } else {
                let Some((ordinal, type_def)) =
                    Assembly::find_in_references(references, namespace, simple)
                else {
                    continue;
                };
                let Ok(ordinal) = u8::try_from(ordinal) else {
                    continue;
                };
                (BodyOwner::Reference(ordinal), type_def)
            };
            seen.insert(instantiation.name.clone());
            for method in type_def.methods() {
                let declaration_only =
                    method.body().is_none() && method.is_abstract() && !type_def.is_interface();
                if method.body().is_none() && !declaration_only {
                    continue;
                }
                let Some(method_name) = method.name() else {
                    continue;
                };
                let Some(signature) = method.signature() else {
                    continue;
                };
                bodies.push(MonoBody {
                    index: *next,
                    instantiation: instantiation.name.clone(),
                    definition: Some(instantiation.definition.clone()),
                    arguments: Some(arguments.clone()),
                    spec: Token::new(table::TYPE_SPEC, 0),
                    rid: method.rid(),
                    name: Box::from(method_name),
                    parameters: signature.parameters,
                    owner,
                    declaration_only,
                });
                *next += 1;
            }
        }
        Ok(())
    }

    /// and no shipping build path passes references here yet.
    pub fn for_assembly_with_references<'a>(
        assembly: &Assembly<'a>,
        references: &[&Assembly<'a>],
        first_index: u32,
    ) -> Result<MonoPlan, Refusal> {
        let mut bodies = Vec::new();
        let mut seen: BTreeSet<Box<str>> = BTreeSet::new();
        let mut next = first_index;
        for row in 1..=assembly.tables().row_count(table::TYPE_SPEC) {
            let spec = Token::new(table::TYPE_SPEC, row);
            let Some(signature) = assembly.type_spec_signature(spec) else {
                continue;
            };
            let SigType::GenericInst { definition, .. } = &signature else {
                continue;
            };
            let type_arg = sig_to_type_arg(assembly, &signature)?;
            if !type_arg.is_closed() {
                continue;
            }
            let name = type_arg.name().into_boxed_str();
            if !seen.insert(name.clone()) {
                continue;
            }
            let (SigType::Class(token) | SigType::ValueType(token)) = definition.as_ref() else {
                continue;
            };
            let owned = *token;
            let (owner, type_def) = if owned.table() == table::TYPE_DEF {
                match assembly.type_def(owned.row()) {
                    Some(type_def) => (BodyOwner::Own, type_def),
                    None => continue,
                }
            } else if owned.table() == table::TYPE_REF {
                let Some(name) = assembly.type_token_name(owned) else {
                    continue;
                };
                match Assembly::find_in_references(references, name.namespace, name.name) {
                    Some((ordinal, type_def)) => {
                        let Ok(ordinal) = u8::try_from(ordinal) else {
                            continue;
                        };
                        (BodyOwner::Reference(ordinal), type_def)
                    }
                    None => continue,
                }
            } else {
                continue;
            };
            for method in type_def.methods() {
                let declaration_only =
                    method.body().is_none() && method.is_abstract() && !type_def.is_interface();
                if method.body().is_none() && !declaration_only {
                    continue;
                }
                let method_name = method
                    .name()
                    .ok_or_else(|| undecodable("monomorphized method name"))?;
                let parameters = method
                    .signature()
                    .ok_or_else(|| undecodable("monomorphized method signature"))?
                    .parameters;
                bodies.push(MonoBody {
                    index: next,
                    instantiation: name.clone(),
                    definition: None,
                    arguments: None,
                    spec,
                    rid: method.rid(),
                    name: Box::from(method_name),
                    parameters,
                    owner,
                    declaration_only,
                });
                next += 1;
            }
        }
        Self::extend_with_closure(assembly, references, &mut bodies, &mut seen, &mut next)?;
        let axis = Self::method_axis(assembly, references, &mut next)?;
        Ok(MonoPlan {
            bodies,
            method_bodies: axis.method_bodies,
            method_spec_index: axis.index,
            virtual_spec_index: axis.virtual_index,
            virtual_pairs: axis.virtual_pairs,
        })
    }

    /// The GENERIC METHOD half of the plan: one body per distinct `(MethodDef, call-site arguments)`
    /// pair the assembly's `MethodSpec` rows name.
    ///
    /// **THE TABLE IS WALKED, NOT THE CALL SITES.** A `MethodSpec` row is a finite, already-closed
    /// enumeration of every pair the assembly spells, so there is no closure to walk and no
    /// growth-on-a-cycle criterion to re-derive here -- the type axis needs one because an
    /// instantiation's own body can name a further instantiation, and a method's arguments cannot
    /// grow that way. (The runtime tier reached the same conclusion for the same reason.)
    ///
    /// **A VIRTUAL GENERIC METHOD'S OWN PAIR IS PLANNED, AND ITS TOKEN IS PUT SOMEWHERE AN ORDINARY
    /// BIND CANNOT REACH.** A `callvirt` at a `MethodSpec` names ONE `MethodDef` -- the declaration,
    /// not the override -- so the body lowered from that token is the BASE's, and binding it is a
    /// program that links, runs and calls the wrong method on a derived receiver. The runtime tier
    /// measured exactly that: without the guard, zero violations and the answer 10 where 20 was
    /// correct.
    ///
    /// **THE GUARD IS NOT "DO NOT PLAN IT", BECAUSE THAT FORBIDS THE CORRECT PROGRAM TOO** -- a base
    /// receiver holding a base object has to reach that very body, so a rule stated that way carries
    /// a correctness property and a scope boundary in one sentence with nothing separating them. It
    /// is instead the pair of [`Self::virtual_spec_index`] and [`Program::close_over_overrides`]:
    ///
    /// * the base's body is planned, but its token lands in a map no ordinary caller reads;
    /// * every OVERRIDE at the same arguments is planned too -- bodies named by nothing anywhere,
    ///   which is the whole reason they must be computed rather than read off a table.
    ///
    /// A tier that reaches these through a dispatch table answers correctly; one that does not
    /// reach them at all refuses, because the only token in play is in the map it does not read.
    ///
    /// Everything else declined here is declined by ABSENCE, which is safe on this tier: an
    /// unplanned `MethodSpec` has no binding, and `resolve` then answers `UnresolvedCall` rather
    /// than falling back to the open definition.
    fn method_axis<'a>(
        assembly: &Assembly<'a>,
        references: &[&Assembly<'a>],
        next: &mut u32,
    ) -> Result<MethodAxis, Refusal> {
        let mut method_bodies: Vec<MonoMethodBody> = Vec::new();
        let mut index: Vec<(Token, u32)> = Vec::new();
        let mut virtual_index: Vec<(Token, u32)> = Vec::new();
        let mut virtual_pairs: Vec<MethodPair> = Vec::new();
        let mut named: Vec<MethodPair> = Vec::new();
        for row in 1..=assembly.tables().row_count(table::METHOD_SPEC) {
            let spec = Token::new(table::METHOD_SPEC, row);
            let (Some(method), Some(arguments)) = (
                assembly.method_spec_method(spec),
                assembly.method_spec_instantiation(spec),
            ) else {
                continue;
            };
            let (owner, rid, definition) = if method.table() == table::METHOD_DEF {
                match assembly.method(method.row()) {
                    Some(definition) => (BodyOwner::Own, method.row(), definition),
                    None => continue,
                }
            } else if method.table() == table::MEMBER_REF {
                match Self::imported_generic_method(assembly, references, method) {
                    Some((ordinal, rid, definition)) => {
                        (BodyOwner::Reference(ordinal), rid, definition)
                    }
                    None => continue,
                }
            } else {
                continue;
            };
            let mut spelled = Vec::new();
            let mut open = false;
            for argument in &arguments {
                let arg = sig_to_type_arg(assembly, argument)?;
                if !arg.is_closed() {
                    open = true;
                    break;
                }
                spelled.push(arg.name());
            }
            if open {
                continue;
            }
            let name = definition
                .name()
                .ok_or_else(|| undecodable("generic method name"))?;
            let declaring_assembly = Self::owning_assembly(assembly, references, owner);
            let seed = Self::seed_pair(declaring_assembly, &definition, name, rid, &arguments);
            if let Some(pair) = &seed {
                named.push(pair.clone());
                if definition.is_virtual() && !virtual_pairs.contains(pair) {
                    virtual_pairs.push(pair.clone());
                }
            }
            if definition.body().is_none() {
                continue;
            }
            let instantiation = alloc::format!("{name}[{}]", spelled.join(",")).into_boxed_str();
            let declaring = Self::declaring_name(declaring_assembly, rid);
            let at = Self::plan_body(
                &mut method_bodies,
                next,
                rid,
                owner,
                name,
                declaring,
                arguments,
                instantiation,
            );
            if definition.is_virtual() {
                virtual_index.push((spec, at));
            } else {
                index.push((spec, at));
            }
        }
        Self::plan_overrides(assembly, references, &named, &mut method_bodies, next)?;
        Ok(MethodAxis {
            method_bodies,
            index,
            virtual_index,
            virtual_pairs,
        })
    }

    /// Adds one monomorphized method body, or returns the index of the one already planned.
    ///
    /// **DEDUPLICATED BY THE PAIR, NOT BY THE ROW.** Two `MethodSpec` rows naming the same method
    /// with the same arguments are one body and both tokens map to it. The OWNER is part of the key,
    /// because one rid in two assemblies is two methods.
    #[allow(clippy::too_many_arguments)]
    fn plan_body(
        method_bodies: &mut Vec<MonoMethodBody>,
        next: &mut u32,
        rid: u32,
        owner: BodyOwner,
        name: &str,
        declaring: Box<str>,
        arguments: Vec<SigType>,
        instantiation: Box<str>,
    ) -> u32 {
        if let Some(body) = method_bodies
            .iter()
            .find(|body| body.rid == rid && body.owner == owner && body.arguments == arguments)
        {
            return body.index;
        }
        let at = *next;
        *next += 1;
        method_bodies.push(MonoMethodBody {
            index: at,
            rid,
            name: Box::from(name),
            arguments,
            declaring,
            instantiation,
            owner,
        });
        at
    }

    /// The assembly a [`BodyOwner`] names, out of the program and its references.
    ///
    /// One place, because the ordinal's meaning -- `Reference(0)` is `references[0]`, and the
    /// program is not in that list at all -- is the kind of off-by-one that reads a real method out
    /// of the wrong assembly rather than failing.
    fn owning_assembly<'x, 'a>(
        assembly: &'x Assembly<'a>,
        references: &'x [&'x Assembly<'a>],
        owner: BodyOwner,
    ) -> &'x Assembly<'a> {
        match owner {
            BodyOwner::Own => assembly,
            BodyOwner::Reference(ordinal) => references
                .get(ordinal as usize)
                .copied()
                .unwrap_or(assembly),
        }
    }

    /// The full name of the type declaring the method at `rid`, or an empty string when the row has
    /// no owner this assembly can name.
    ///
    /// **THE OWNER IS FOUND BY ASKING THE TYPES, NOT BY RESOLVING A NAME.** II.22.37 puts the
    /// method list on the TypeDef side as a RANGE, so there is no reverse index and a scan is the
    /// honest form. Going the other way -- reading the method's declaring-type NAME and looking it
    /// up -- would pass through a flat `(namespace, name)` pair, which II.22.38 makes ambiguous for
    /// a nested type: `Widget.Nested` and `Gadget.Nested` both read `('', 'Nested')`. A wrong owner
    /// here does not fail; it files a body under another type's name.
    fn declaring_name(assembly: &Assembly<'_>, rid: u32) -> Box<str> {
        assembly
            .type_defs()
            .find(|type_def| type_def.methods().any(|method| method.rid() == rid))
            .and_then(|type_def| type_def_full_name(assembly, type_def.token()))
            .unwrap_or_default()
            .into_boxed_str()
    }

    /// One `MethodSpec` pair as the override closure's input, or `None` when the definition's
    /// signature does not decode.
    ///
    /// **AN UNDECODABLE SIGNATURE YIELDS NO SEED RATHER THAN A FABRICATED ONE**, for
    /// `decodable_params`' reason one level out: [`Program::close_over_overrides`] matches on the
    /// declared parameter list, so a seed carrying an empty one would select as "the override" any
    /// same-named nullary method in the hierarchy.
    fn seed_pair(
        assembly: &Assembly<'_>,
        definition: &Method<'_>,
        name: &str,
        rid: u32,
        arguments: &[SigType],
    ) -> Option<MethodPair> {
        let signature = definition.signature()?;
        let declaring = Self::declaring_name(assembly, rid);
        if declaring.is_empty() {
            return None;
        }
        Some(MethodPair {
            declaring,
            method: Box::from(name),
            parameters: signature.parameters,
            arity: signature.generic_param_count,
            arguments: arguments.to_vec(),
        })
    }

    /// Plans a body for every OVERRIDE of a named pair -- [`Program::close_over_overrides`]'s
    /// additions, resolved back to the `MethodDef` each one names.
    ///
    /// # Why this is a consumer and not a second closure
    ///
    /// The override rule has ONE implementation, in this crate, and both tiers consume it. A second
    /// walk written beside it would let the two tiers' answers about which method overrides which
    /// drift apart silently, and neither tier's tests can see the other's.
    ///
    /// # What the closure's stated boundary means for this caller
    ///
    /// Two things it does NOT cover, and neither may be papered over here:
    ///
    /// * A definition in an assembly the [`Program`] was not given is not walked. The `Program` is
    ///   built over the module and its references, which is every assembly this build can lower
    ///   from, so an unwalked definition is one whose body could not be emitted either way.
    /// * An EXPLICIT interface implementation is named through `MethodImpl` under a mangled name
    ///   and is a third LOOKUP. It is absent from the additions, so a dispatch built on them must
    ///   refuse that shape rather than read this set as covering it.
    ///
    /// # The additions carry no token, which is what makes them safe to add
    ///
    /// Nothing anywhere names `Derived::Tag<int>` -- no `MethodSpec` row, no `MemberRef` -- so no
    /// entry is made in `method_spec_index` and no call site can bind to one by token. A dispatch
    /// arm reaches these bodies through a table or not at all.
    fn plan_overrides<'a>(
        assembly: &Assembly<'a>,
        references: &[&Assembly<'a>],
        named: &[MethodPair],
        method_bodies: &mut Vec<MonoMethodBody>,
        next: &mut u32,
    ) -> Result<(), Refusal> {
        if named.is_empty() {
            return Ok(());
        }
        let assemblies: Vec<Assembly<'a>> = core::iter::once(*assembly)
            .chain(references.iter().map(|reference| **reference))
            .collect();
        let program = Program::new(&assemblies);
        for addition in program.close_over_overrides(named) {
            let Some((owner, rid, definition)) =
                Self::override_definition(assembly, references, &addition)
            else {
                continue;
            };
            if definition.body().is_none() {
                continue;
            }
            let mut spelled = Vec::new();
            for argument in &addition.arguments {
                spelled.push(sig_to_type_arg(assembly, argument)?.name());
            }
            let instantiation =
                alloc::format!("{}[{}]", addition.method, spelled.join(",")).into_boxed_str();
            Self::plan_body(
                method_bodies,
                next,
                rid,
                owner,
                &addition.method,
                addition.declaring.clone(),
                addition.arguments.clone(),
                instantiation,
            );
        }
        Ok(())
    }

    /// The `MethodDef` an override pair names: where it lives, its rid there, and the method.
    ///
    /// The match is the closure's own -- name, generic arity and declared parameters, compared
    /// structurally -- because this is re-finding what the closure already found. `MethodPair`
    /// carries the type NAME rather than a row, so the row has to be recovered, and recovering it
    /// by a DIFFERENT rule is how the two sides come to disagree about which method was meant.
    ///
    /// The program is searched before its references, which is [`Program::new`]'s own first-wins
    /// precedence rather than a second one written here.
    fn override_definition<'a>(
        assembly: &Assembly<'a>,
        references: &[&Assembly<'a>],
        pair: &MethodPair,
    ) -> Option<(BodyOwner, u32, Method<'a>)> {
        let candidates = core::iter::once(assembly).chain(references.iter().copied());
        for (ordinal, candidate) in candidates.enumerate() {
            for type_def in candidate.type_defs() {
                if type_def_full_name(candidate, type_def.token()).as_deref()
                    != Some(pair.declaring.as_ref())
                {
                    continue;
                }
                for method in type_def.methods() {
                    if method.name() != Some(pair.method.as_ref()) {
                        continue;
                    }
                    let Some(signature) = method.signature() else {
                        continue;
                    };
                    if signature.generic_param_count != pair.arity
                        || signature.parameters != pair.parameters
                    {
                        continue;
                    }
                    let owner = match ordinal {
                        0 => BodyOwner::Own,
                        _ => BodyOwner::Reference(u8::try_from(ordinal - 1).ok()?),
                    };
                    return Some((owner, method.rid(), method));
                }
            }
        }
        None
    }

    /// The generic method a cross-assembly `MethodSpec` names: its owner's ordinal, its `MethodDef`
    /// rid THERE, and the method itself -- or `None` when it does not resolve to exactly one.
    ///
    /// # Why the match is by shape and not by signature
    ///
    /// A `MemberRef`'s blob carries the DEFINITION's signature spelled in the CALLER's token space,
    /// so its `Class`/`ValueType` tokens mean nothing in the owner's tables and the two sides cannot
    /// be compared type by type. What CAN be compared is the shape both sides agree on without
    /// resolving a token: the name, the number of type parameters the method declares (II.23.2.1 --
    /// **binding-significant**, `M<T>()` and `M<T,U>()` are different methods) and the number of
    /// parameters.
    ///
    /// **AMBIGUITY IS A REFUSAL, NOT A FIRST-MATCH.** Two overloads sharing that shape differ only
    /// in parameter TYPES, which is exactly what cannot be compared here -- so binding to the first
    /// would be a coin flip that links, runs and calls the wrong method. Declining leaves the call
    /// site unbound, which fails loud.
    ///
    /// **BOTH HALVES ARE RED-PROVED BY ONE PERTURBATION, AND THE FIXTURE WAS BUILT FOR IT.**
    /// `genmethlib` declares an `Echo<T>(T, T)` that nothing calls. Drop the shape comparison and
    /// the two `Echo` candidates make this refuse; the call site then finds no binding and the build
    /// fails LOUD, with every other pair in the gate unmoved. Without that sibling the comparison
    /// could be deleted with every row still green.
    fn imported_generic_method<'a>(
        assembly: &Assembly<'a>,
        references: &[&Assembly<'a>],
        method: Token,
    ) -> Option<(u8, u32, Method<'a>)> {
        let member = assembly.member_ref(method.row())?;
        let name = member.name()?;
        let signature = member.method_signature()?;
        let parent = assembly.type_token_name(member.parent())?;
        let (ordinal, type_def) =
            Assembly::find_in_references(references, parent.namespace, parent.name)?;
        let ordinal = u8::try_from(ordinal).ok()?;
        let mut found = None;
        for candidate in type_def.methods() {
            if candidate.name() != Some(name) {
                continue;
            }
            let Some(candidate_signature) = candidate.signature() else {
                continue;
            };
            if candidate_signature.generic_param_count != signature.generic_param_count
                || candidate_signature.parameters.len() != signature.parameters.len()
                || candidate_signature.has_this != signature.has_this
            {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some((ordinal, candidate.rid(), candidate));
        }
        found
    }

    /// The function index the call site at `spec` (a `MethodSpec` token) binds to, or `None` when
    /// this plan does not lower that pair -- in which case the call site refuses rather than binding
    /// to the open definition.
    #[must_use]
    pub fn method_index_of(&self, spec: Token) -> Option<u32> {
        self.method_spec_index
            .iter()
            .find(|(token, _)| *token == spec)
            .map(|(_, index)| *index)
    }

    /// The index of the body the `MethodSpec` at `spec` names when that pair is VIRTUAL -- the
    /// BASE's declaration, which is the right body only for a receiver of the declaring type itself.
    ///
    /// **A CALLER THAT BINDS THIS DIRECTLY FROM A `callvirt` HAS WRITTEN THE DEFECT THE WHOLE
    /// SPLIT EXISTS TO PREVENT**, and it is a quiet one: the program builds, links, runs and answers
    /// with the base's implementation. Use it for a non-virtual `call` -- `base.Tag<int>(x)`, where
    /// binding exactly what the token names IS the semantics -- and otherwise only to type the call,
    /// with [`Self::virtual_method_instantiations`] supplying the dispatch.
    #[must_use]
    pub fn virtual_method_index_of(&self, spec: Token) -> Option<u32> {
        self.virtual_spec_index
            .iter()
            .find(|(token, _)| *token == spec)
            .map(|(_, index)| *index)
    }

    /// The distinct argument lists a virtual generic method is called at anywhere in the module, in
    /// `MethodSpec` table order -- one dispatch slot each, for every type in its hierarchy.
    ///
    /// **PROGRAM-GLOBAL BY CONSTRUCTION, WHICH IS THE PROPERTY A VTABLE NEEDS.** The order and the
    /// membership come from the module's own table, so `Base` and `Derived` expand one declaration
    /// into the same slots in the same order -- and a `callvirt` computing its slot in `Base`'s
    /// numbering indexes the right entry of `Derived`'s table. Answering "the arguments this type
    /// has a body for" instead would renumber the moment a type in the middle failed to override.
    ///
    /// The key is the declaration's identity as ECMA-335 II.9.9 defines it for overriding -- name,
    /// generic ARITY and declared parameters -- and not the declaring type, because the whole point
    /// is that several types share these slots.
    ///
    /// **THE PRICE IS PAID IN SLOTS AND NEVER IN CORRECTNESS.** Two UNRELATED hierarchies declaring
    /// the same signature share this list, so each expands into the union of both their argument
    /// lists and the surplus slots hold the open definition's rid -- a `stub()` nothing dispatches
    /// to, since a call site can only name an argument list that seeded the list in the first place.
    /// Narrowing the key to the hierarchy would save those slots and reintroduce exactly the
    /// disagreement the paragraph above describes, which is a bad trade at ~58 B a slot.
    #[must_use]
    pub fn virtual_method_instantiations(
        &self,
        method: &str,
        arity: u32,
        parameters: &[SigType],
    ) -> Vec<&[SigType]> {
        let mut found: Vec<&[SigType]> = Vec::new();
        for pair in &self.virtual_pairs {
            if pair.method.as_ref() != method
                || pair.arity != arity
                || pair.parameters != parameters
            {
                continue;
            }
            if !found.iter().any(|seen| *seen == pair.arguments.as_slice()) {
                found.push(&pair.arguments);
            }
        }
        found
    }

    /// The index of the body `declaring` contributes for `method` at `arguments`, or `None` when
    /// this type has none -- an abstract declaration, or a type that does not override.
    ///
    /// This is the mapping a dispatch slot is filled from, and it is keyed by the DECLARING TYPE
    /// precisely where [`Self::virtual_method_instantiations`] is not: which slots exist is a
    /// property of the program, which body sits in one is a property of the type.
    #[must_use]
    pub fn virtual_method_body(
        &self,
        declaring: &str,
        method: &str,
        arguments: &[SigType],
    ) -> Option<u32> {
        self.method_bodies
            .iter()
            .find(|body| {
                body.declaring.as_ref() == declaring
                    && body.name.as_ref() == method
                    && body.arguments == arguments
            })
            .map(|body| body.index)
    }

    /// Every generic-METHOD body to emit, in index order.
    #[must_use]
    pub fn method_bodies(&self) -> &[MonoMethodBody] {
        &self.method_bodies
    }

    /// The function index a call on `instantiation` naming `name` with `parameters` binds to, or
    /// `None` when this plan does not carry that body.
    #[must_use]
    pub fn index_of(&self, instantiation: &str, name: &str, parameters: &[SigType]) -> Option<u32> {
        self.bodies
            .iter()
            .find(|body| {
                &*body.instantiation == instantiation
                    && &*body.name == name
                    && body.parameters == parameters
            })
            .map(|body| body.index)
    }

    /// The `(definition full name, closed arguments)` a CLOSURE-FOUND instantiation carries, or
    /// `None` for one the module spells through a `TypeSpec` row of its own.
    ///
    /// **THE DESCRIPTOR PATH NEEDS THIS AND CANNOT GET IT FROM THE ROW**, because there is no row:
    /// an instantiation reached only through another body's CIL is named nowhere in the module's
    /// tables. A consumer that decodes [`MonoBody::spec`] alone gives such a type BODIES AND NO
    /// DESCRIPTOR -- it builds, links, and then dispatches through a vtable that was never laid.
    #[must_use]
    pub fn carried(&self, instantiation: &str) -> Option<(&str, &[SigType])> {
        self.bodies.iter().find_map(|body| {
            (&*body.instantiation == instantiation)
                .then(|| {
                    Some((
                        &**body.definition.as_ref()?,
                        &**body.arguments.as_ref()?,
                    ))
                })
                .flatten()
        })
    }

    /// Every body to emit, in index order.
    #[must_use]
    pub fn bodies(&self) -> &[MonoBody] {
        &self.bodies
    }

    /// The distinct INSTANTIATIONS this plan covers -- `(canonical spelling, the TypeSpec naming
    /// it)` -- in first-appearance order, one entry per type rather than one per body.
    ///
    /// This is the population a DESCRIPTOR is owed for: a body is per method, a descriptor is per
    /// TYPE, and emitting one per body would lay the same descriptor several times.
    #[must_use]
    pub fn instantiations(&self) -> Vec<(&str, Token)> {
        let mut out: Vec<(&str, Token)> = Vec::new();
        for body in &self.bodies {
            if !out.iter().any(|(name, _)| *name == &*body.instantiation) {
                out.push((&body.instantiation, body.spec));
            }
        }
        out
    }

    /// How many bodies this plan emits, both axes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bodies.len() + self.method_bodies.len()
    }

    /// Whether this plan emits nothing -- which is the case for every non-generic program, and is
    /// what keeps the ordinary path untouched.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty() && self.method_bodies.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_metadata::signature::calling;

    fn class(name: &str) -> TypeArg {
        TypeArg::Named {
            name: name.to_owned().into_boxed_str(),
            value_type: false,
        }
    }

    /// A plan entry, so the LOOKUP can be tested without a generic-bearing assembly. Building the
    /// plan needs one (no fixture in this tree declares generics -- see
    /// `examples/dump-mono-bodies`); deciding which entry a call site binds to does not, and that
    /// decision is where a wrong answer would be silent.
    fn body(index: u32, instantiation: &str, name: &str, parameters: Vec<SigType>) -> MonoBody {
        MonoBody {
            index,
            instantiation: instantiation.to_owned().into_boxed_str(),
            definition: None,
            arguments: None,
            spec: Token::new(table::TYPE_SPEC, 1),
            rid: 4,
            name: name.to_owned().into_boxed_str(),
            parameters,
            owner: BodyOwner::Own,
            declaration_only: false,
        }
    }

    /// THE CONTROL IS THE PAIR, NOT EITHER ROW. Two instantiations of ONE definition share a
    /// method name, a parameter signature AND a MethodDef rid -- everything except the type
    /// argument. A lookup keyed on anything less than the instantiation's canonical spelling
    /// answers the same index for both, which is one body serving two types: the wrong GC trace map
    /// for one of them, and no size anywhere can see it.
    #[test]
    fn two_instantiations_of_one_definition_bind_to_different_bodies() {
        let plan = MonoPlan {
            bodies: alloc::vec![
                body(11, "Box`1[System.Int32]", "Get", Vec::new()),
                body(12, "Box`1[System.String]", "Get", Vec::new()),
            ],
            ..MonoPlan::default()
        };
        assert_eq!(plan.index_of("Box`1[System.Int32]", "Get", &[]), Some(11));
        assert_eq!(plan.index_of("Box`1[System.String]", "Get", &[]), Some(12));
        assert_eq!(plan.index_of("Box`1[System.Int64]", "Get", &[]), None);
    }

    /// The parameters are half the key because a name alone binds an overload to its sibling -- the
    /// fabricated-nullary collision one layer out. Both rows here are the SAME instantiation and the
    /// SAME method name, so only the signature separates them.
    #[test]
    fn an_overload_binds_by_its_parameters_and_not_by_its_name() {
        let plan = MonoPlan {
            bodies: alloc::vec![
                body(11, "Box`1[System.Int32]", "Set", alloc::vec![SigType::Var(0)]),
                body(
                    12,
                    "Box`1[System.Int32]",
                    "Set",
                    alloc::vec![SigType::Var(0), SigType::I4]
                ),
            ],
            ..MonoPlan::default()
        };
        assert_eq!(
            plan.index_of("Box`1[System.Int32]", "Set", &[SigType::Var(0)]),
            Some(11)
        );
        assert_eq!(
            plan.index_of(
                "Box`1[System.Int32]",
                "Set",
                &[SigType::Var(0), SigType::I4]
            ),
            Some(12)
        );
        assert_eq!(plan.index_of("Box`1[System.Int32]", "Set", &[]), None);
    }

    /// The property that keeps every non-generic program byte-identical: an empty plan answers
    /// nothing, so the resolver's monomorphized arm cannot fire and the call falls through to the
    /// path it always took.
    #[test]
    fn an_empty_plan_answers_nothing() {
        let plan = MonoPlan::default();
        assert!(plan.is_empty());
        assert_eq!(plan.len(), 0);
        assert_eq!(plan.index_of("Box`1[System.Int32]", "Get", &[]), None);
    }

    fn list_of(argument: TypeArg) -> TypeArg {
        TypeArg::Instance {
            definition: "System.Collections.Generic.List`1"
                .to_owned()
                .into_boxed_str(),
            value_type: false,
            arguments: alloc::vec![argument],
        }
    }

    /// The spelling is .NET's own `Type.ToString()`. Each expectation here was READ OFF a running
    /// .NET 8 rather than recalled -- `typeof(T).ToString()` for the same type.
    #[test]
    fn spelling_matches_the_dotnet_oracle() {
        assert_eq!(
            list_of(TypeArg::Primitive(element::I4)).name(),
            "System.Collections.Generic.List`1[System.Int32]"
        );
        assert_eq!(
            list_of(list_of(TypeArg::Primitive(element::I4))).name(),
            "System.Collections.Generic.List`1[System.Collections.Generic.List`1[System.Int32]]"
        );
        assert_eq!(
            TypeArg::Instance {
                definition: "System.Collections.Generic.Dictionary`2"
                    .to_owned()
                    .into_boxed_str(),
                value_type: false,
                arguments: alloc::vec![
                    TypeArg::Primitive(element::STRING),
                    TypeArg::Primitive(element::I4)
                ],
            }
            .name(),
            "System.Collections.Generic.Dictionary`2[System.String,System.Int32]"
        );
        assert_eq!(
            list_of(TypeArg::SzArray(Box::new(TypeArg::Primitive(element::I4)))).name(),
            "System.Collections.Generic.List`1[System.Int32[]]"
        );
        assert_eq!(
            list_of(TypeArg::Array {
                element: Box::new(TypeArg::Primitive(element::I4)),
                rank: 3
            })
            .name(),
            "System.Collections.Generic.List`1[System.Int32[,,]]"
        );
        assert_eq!(
            list_of(TypeArg::Pointer(Box::new(TypeArg::Primitive(element::I4)))).name(),
            "System.Collections.Generic.List`1[System.Int32*]"
        );
        assert_eq!(
            TypeArg::ByRef(Box::new(TypeArg::Primitive(element::I4))).name(),
            "System.Int32&"
        );
    }

    /// Every built-in spells under its BCL name, never its C# keyword.
    #[test]
    fn primitives_spell_as_the_bcl_names() {
        for (byte, name) in [
            (element::VOID, "System.Void"),
            (element::BOOLEAN, "System.Boolean"),
            (element::CHAR, "System.Char"),
            (element::I1, "System.SByte"),
            (element::U1, "System.Byte"),
            (element::I2, "System.Int16"),
            (element::U2, "System.UInt16"),
            (element::I4, "System.Int32"),
            (element::U4, "System.UInt32"),
            (element::I8, "System.Int64"),
            (element::U8, "System.UInt64"),
            (element::R4, "System.Single"),
            (element::R8, "System.Double"),
            (element::STRING, "System.String"),
            (element::OBJECT, "System.Object"),
            (element::I, "System.IntPtr"),
            (element::U, "System.UIntPtr"),
            (element::TYPEDBYREF, "System.TypedReference"),
        ] {
            assert_eq!(TypeArg::Primitive(byte).name(), name);
        }
    }

    /// THE FREEZE ITEM, AS A CONTROL RATHER THAN AN EXERCISE. The collapsed spelling and the
    /// by-name spelling are separated here: under a shared reference marker the two names below
    /// would be EQUAL, and `o is IList<string>` would answer true for a type implementing only
    /// `IList<Foo>`. A test that only checked `IList<int>` against `IList<string>` would pass under
    /// BOTH candidates, since a value argument is not what the shortcut collapses.
    ///
    /// **The half that asserts the INTERFACE TAG consumes this spelling lives in `lamella-aot`**
    /// (`resolver::tests::an_instantiations_interface_tag_takes_the_canonical_spelling`), because
    /// `interface_method_tag` is that crate's. The claim spans two crates, so it takes two tests,
    /// and each names the other.
    #[test]
    fn instantiations_are_distinct_interfaces() {
        let of_string = TypeArg::Instance {
            definition: "System.Collections.Generic.IList`1"
                .to_owned()
                .into_boxed_str(),
            value_type: false,
            arguments: alloc::vec![TypeArg::Primitive(element::STRING)],
        };
        let of_foo = TypeArg::Instance {
            definition: "System.Collections.Generic.IList`1"
                .to_owned()
                .into_boxed_str(),
            value_type: false,
            arguments: alloc::vec![class("Sample.Foo")],
        };
        let string_name = of_string.name();
        let foo_name = of_foo.name();
        assert_ne!(string_name, foo_name);
        assert_ne!(
            exception_tag_for_name("", &string_name),
            exception_tag_for_name("", &foo_name)
        );
        assert!(!"System.Collections.ArrayList".contains('['));
    }

    /// THE SPELLING RULE'S FINGERPRINT, PINNED. It is a wire contract between a baked image and a
    /// separately-loaded PE: one character of disagreement is two types where there should be one.
    ///
    /// **A moved value is not automatically a failure -- it is a prompt.** If the rule changed on
    /// purpose, update the literal AND say so where consumers can see it, because the artifacts that
    /// carry the old spelling do not update themselves.
    ///
    /// **THE LITERAL WAS READ OFF THIS IMPLEMENTATION, SO IT PROVES NOTHING ON ITS OWN** -- a
    /// value pinned to whatever the code already did is the shape this project rules against. What
    /// makes it meaningful is the pair of tests either side of it, and they are independent of it:
    /// [`spelling_matches_the_dotnet_oracle`] checks the rule against an EXTERNAL authority (a
    /// running .NET 8), and [`the_fingerprint_moves_for_every_clause_of_the_rule`] checks that this
    /// number can SEE each clause. The literal only carries that verified state forward in time.
    #[test]
    fn the_spelling_rule_fingerprint_is_pinned() {
        assert_eq!(
            spelling_rule_fingerprint(),
            0x8647_0575,
            "the canonical instantiation spelling CHANGED -- this is a cross-artifact contract"
        );
    }

    /// A PINNED CONSTANT PROVES NOTHING UNLESS IT MOVES WHEN THE RULE DOES, and the failure mode
    /// of a fingerprint is that it is blind to the clause someone actually changes. So each clause
    /// is perturbed here and the fingerprint must move for every one.
    ///
    /// This is the control the pin above cannot be. A corpus that happened to omit, say, the array
    /// rank would leave `[,,]` free to become `[3]` with the pin still green -- which is exactly how
    /// a contract stops being one.
    #[test]
    fn the_fingerprint_moves_for_every_clause_of_the_rule() {
        let base = spelling_rule_fingerprint();
        let int = TypeArg::Primitive(element::I4);
        let list = |arguments: Vec<TypeArg>| TypeArg::Instance {
            definition: "N.List`1".to_owned().into_boxed_str(),
            value_type: false,
            arguments,
        };
        let clauses: alloc::vec::Vec<(String, &str)> = alloc::vec![
            (list(alloc::vec![int.clone()]).name(), "N.List`1<System.Int32>"),
            (
                TypeArg::Instance {
                    definition: "N.Pair`2".to_owned().into_boxed_str(),
                    value_type: false,
                    arguments: alloc::vec![int.clone(), TypeArg::Primitive(element::STRING)],
                }
                .name(),
                "N.Pair`2[System.String,System.Int32]"
            ),
            (
                list(alloc::vec![TypeArg::Array {
                    element: Box::new(int.clone()),
                    rank: 3
                }])
                .name(),
                "N.List`1[System.Int32[3]]"
            ),
            (
                list(alloc::vec![TypeArg::SzArray(Box::new(int.clone()))]).name(),
                "N.List`1[System.Int32[0..]]"
            ),
            (
                list(alloc::vec![TypeArg::ByRef(Box::new(int.clone()))]).name(),
                "N.List`1[ref System.Int32]"
            ),
            (
                TypeArg::Instance {
                    definition: "N.Outer`1+Inner`1".to_owned().into_boxed_str(),
                    value_type: false,
                    arguments: alloc::vec![int.clone(), int.clone()],
                }
                .name(),
                "N.Outer`1.Inner`1[System.Int32,System.Int32]"
            ),
            (
                list(alloc::vec![TypeArg::Var(0)]).name(),
                "N.List`1[T]"
            ),
            (int.name(), "int"),
        ];
        for (produced, alternative) in &clauses {
            assert_ne!(
                produced.as_str(),
                *alternative,
                "the rule already produces the alternative -- this clause is not being tested"
            );
            assert!(
                !produced.is_empty(),
                "a clause that spells to nothing cannot be pinned"
            );
        }
        let mut shortened = 0x811c_9dc5u32;
        shortened = fnv1a32(shortened, list(alloc::vec![int.clone()]).name().as_bytes());
        shortened = fnv1a32(shortened, b"\n");
        assert_ne!(base, shortened, "the fingerprint must depend on its corpus");
    }

    /// A `MethodSpec`'s arguments are CONSECUTIVE in one blob, so the decoder must stop each type at
    /// its own end and not run into the next. This module depends on that and no longer implements
    /// it, so the pin is on the CONTRACT rather than on the code.
    ///
    /// **`C<int>` followed by `string` is the row that matters.** A `GenericInst` is the only
    /// shape whose length depends on a COUNT read from inside the blob, so a decoder that guessed
    /// would land mid-signature and every argument after it would be wrong -- a plausible wrong walk
    /// rather than an error. A blob of ONE argument cannot catch that; the second argument is what
    /// proves the first one ended where it should.
    #[test]
    fn a_method_specs_arguments_do_not_run_into_each_other() {
        let token = 13u8;
        let arguments = parse_method_spec(&[
            calling::GENERICINST,
            3,
            element::GENERICINST,
            element::CLASS,
            token,
            1,
            element::I4,
            element::STRING,
            element::SZARRAY,
            element::I4,
        ])
        .expect("a well-formed MethodSpec blob");
        assert_eq!(arguments.len(), 3);
        assert!(matches!(arguments[0], SigType::GenericInst { .. }));
        assert_eq!(arguments[1], SigType::String);
        assert_eq!(arguments[2], SigType::SzArray(Box::new(SigType::I4)));
        assert!(parse_method_spec(&[calling::GENERICINST, 2, element::I4]).is_err());
        assert_eq!(calling::GENERICINST, element::I8);
        assert!(parse_method_spec(&[element::I4, 1, element::I4]).is_err());
    }

    /// Depth is the quantity growth-on-a-cycle compares, so it must separate `C<C<int>>` from
    /// `C<int>` while leaving `C<int>` and `C<string>` equal.
    #[test]
    fn depth_measures_argument_nesting_not_recursion() {
        assert_eq!(list_of(TypeArg::Primitive(element::I4)).depth(), 1);
        assert_eq!(list_of(list_of(TypeArg::Primitive(element::I4))).depth(), 2);
        assert_eq!(
            list_of(TypeArg::Primitive(element::I4)).depth(),
            list_of(TypeArg::Primitive(element::STRING)).depth()
        );
        assert_eq!(
            TypeArg::SzArray(Box::new(list_of(TypeArg::Primitive(element::I4)))).depth(),
            1
        );
    }

    /// Substitution replaces `!n` and refuses a parameter it has no argument for, rather than
    /// defaulting -- the same rule the undecodable-signature guard follows.
    #[test]
    fn substitution_refuses_a_parameter_it_cannot_close() {
        let open = list_of(TypeArg::Var(0));
        assert!(!open.is_closed());
        let closed = open
            .substitute(&[TypeArg::Primitive(element::I4)], &[])
            .expect("!0 has an argument");
        assert!(closed.is_closed());
        assert_eq!(closed.name(), "System.Collections.Generic.List`1[System.Int32]");
        assert!(list_of(TypeArg::Var(1)).substitute(&[TypeArg::Primitive(element::I4)], &[]).is_none());
        assert!(list_of(TypeArg::MVar(0)).substitute(&[TypeArg::Primitive(element::I4)], &[]).is_none());
    }
}
