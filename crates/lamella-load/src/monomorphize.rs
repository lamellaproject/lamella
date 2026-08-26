//! Bake-time monomorphization: the SUBSTITUTION AND EMISSION half.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use lamella_cil::Opcode;
use lamella_cil_runtime::module::RawCil;
use lamella_cil_runtime::{MethodId, Module, TypeId, Value};
use lamella_metadata::{Assembly, SigType};
use lamella_token::Token;

use super::{
    FIELD, FieldNameIndex, MEMBER_REF, METHOD_DEF, METHOD_SPEC, TYPE_DEF, TYPE_REF, TYPE_SPEC,
    TypeNameIndex,
    arg_count, cast_elem_of_sig, default_field_value, is_special_reference_base,
    type_key, type_name_key,
};

/// The metadata table id the rewritten tokens live in.
///
/// ECMA-335 assigns table ids up to `0x2C` and the heap-token ids `0x70`..`0x72`, so `0x7E` names
/// nothing a reader can produce. That matters: a synthetic token must be one no real row could
/// collide with, since the tables it keys are shared with the assembly's own tokens.
const SYNTHETIC_TABLE: u8 = 0x7E;

/// Which [`DefinitionSource`] is the program's own. The caller's contract is that it is first, and
/// two things depend on it: a name a reference also declares resolves to the program's, and an
/// instantiation of the program's own definition needs no check on its arguments -- there is one
/// assembly there and nothing to rebase.
const PROGRAM_SOURCE: usize = 0;

/// The name a DEFINITION is keyed by in this pass -- asked of the shared crate's speller rather
/// than spelled a second time here.
///
/// # Why this is not [`super::full_type_name`]
///
/// Every name this pass matches a definition against was written by
/// `lamella_generics::type_def_full_name`: an [`Instantiation`]'s `definition`, a
/// `lamella_generics::MethodPair`'s `declaring`, and the names the closure walk answers to. That
/// function spells a NESTED type as its enclosing chain joined with `+`, prefixed by the OUTERMOST
/// type's namespace. [`super::full_type_name`] spells the row's OWN namespace and name, and a
/// nested `TypeDef`'s namespace is empty (II.22.32) -- so it answers a bare `Cursor` where the
/// set holds `` Box`1+Cursor ``. **The two agree for every top-level type and disagree for
/// every nested one.**
///
/// **AND THE DISAGREEMENT IS SILENT.** A lookup that misses leaves the call site marked, the load
/// succeeds, and the program traps at run time on a call nothing reported. Measured on a
/// `class Box<T>` with four nested types: 4 `DefinitionNotHere` + 11 `InstantiationNotInSet`
/// refusals for a program whose every instantiation WAS in the set, and a
/// `UnresolvedCall(0x0A000012)` on `` Box`1[System.Int32] ``'s own `Drive` -- a method that is not
/// nested in anything.
///
/// # Why the capability-off build answers `None`
///
/// Without `generics` the instantiation set is always EMPTY -- every `load_with_corlib*` entry
/// point passes one -- so no definition is ever looked up and there is no key to spell. Answering
/// `None` keeps ONE speller in the tree rather than growing a second one for the build that cannot
/// consult it.
#[cfg(feature = "generics")]
fn definition_key(assembly: &Assembly<'_>, token: Token) -> Option<String> {
    lamella_generics::type_def_full_name(assembly, token)
}

/// The capability-off answer: there is no instantiation set to key against. See the enabled arm.
#[cfg(not(feature = "generics"))]
fn definition_key(_assembly: &Assembly<'_>, _token: Token) -> Option<String> {
    None
}

/// The closed instantiation set `program` requires, collected and spelled by the SHARED collector
/// both tiers consume.
///
/// **THIS FUNCTION IS A BRIDGE, NOT A COLLECTOR.** Membership comes from
/// `lamella_generics::Program::instantiations` -- the closure walk that carries the
/// growth-on-a-cycle refusal -- and every name comes from that crate's canonical spelling. All this
/// does is read the decoded ARGUMENTS back off the `TypeSpec` row the walk's name identifies,
/// because the walk's own arguments are name-keyed (deliberately: a token means nothing outside its
/// assembly) while substitution here needs the assembly-local signatures the row carries.
///
/// An instantiation the walk found that the program names through no `TypeSpec` row is SKIPPED
/// rather than guessed at, and the caller sees it as a call site that stays marked. Silently
/// inventing a signature for it is the one thing that would put a wrong type in the image.
#[cfg(feature = "generics")]
pub fn collect_instantiations<'pe>(
    program: &Assembly<'pe>,
    references: &[Assembly<'pe>],
) -> Vec<Instantiation> {
    let mut assemblies = Vec::with_capacity(1 + references.len());
    assemblies.push(program.clone());
    assemblies.extend(references.iter().cloned());
    let walk = lamella_generics::Program::new(&assemblies);
    let Ok(closed) = walk.instantiations() else {
        return Vec::new();
    };

    let mut program_tokens: BTreeMap<String, Token> = BTreeMap::new();
    for type_ref in program.type_refs() {
        let token = type_ref.token();
        if let Some(name) = lamella_generics::type_def_full_name(program, token) {
            program_tokens.entry(name).or_insert(token);
        }
    }
    for type_def in program.type_defs() {
        let token = type_def.token();
        if let Some(name) = lamella_generics::type_def_full_name(program, token) {
            program_tokens.insert(name, token);
        }
    }

    let mut rows: Vec<(String, (String, Vec<SigType>))> = Vec::new();
    for row in 1..u32::from(u16::MAX) {
        let token = Token::new(TYPE_SPEC, row);
        let Some(signature) = program.type_spec_signature(token) else {
            break;
        };
        let (Some(name), Some(parts)) = (
            lamella_generics::spell_sig(program, &signature),
            lamella_generics::instantiation_of(program, token),
        ) else {
            continue;
        };
        rows.push((name, parts));
    }

    closed
        .into_iter()
        .filter_map(|found| {
            let (definition, arguments) = match rows
                .iter()
                .find(|(name, _)| name.as_str() == found.name.as_ref())
                .map(|(_, parts)| parts.clone())
            {
                Some(parts) => parts,
                None => (
                    found.definition.clone().into_string(),
                    expressible_arguments(&found.arguments, &program_tokens)?,
                ),
            };
            Some(Instantiation {
                definition,
                arguments,
                name: found.name.into_string(),
            })
        })
        .collect()
}

/// The walk's decoded arguments as THIS assembly's signatures: a payload-free element byte carries
/// itself, and a NAMED or CONSTRUCTED argument is resolved through `program_tokens` to the token the
/// assembly being lowered already holds for that name.
///
/// `None` the moment one cannot be expressed -- a type parameter, or a name this assembly has no row
/// for -- because the alternative is a signature naming something the program cannot mean.
///
/// # Why resolving a name here is not inventing a token
///
/// Inventing a token is taking a number from one assembly and reading it against another's tables.
/// This does the opposite: it starts from a NAME, which has no world, and asks the assembly being
/// lowered for its OWN row. A name it has no row for is refused, not guessed.
///
/// The DERIVED set is what this reaches. `List<Alpha>` closes over `ListEnumerator<Alpha>`,
/// `IEnumerable<Alpha>` and `IEnumerator<Alpha>`, and a program spells none of them -- so under a
/// payload-free-only rule each was dropped and the instantiation emitted with a hole exactly where
/// `GetEnumerator` pointed.
#[cfg(feature = "generics")]
fn expressible_arguments(
    arguments: &[lamella_generics::TypeArg],
    program_tokens: &BTreeMap<String, Token>,
) -> Option<Vec<SigType>> {
    arguments
        .iter()
        .map(|argument| expressible_argument(argument, program_tokens))
        .collect()
}

/// One argument, per [`expressible_arguments`].
#[cfg(feature = "generics")]
fn expressible_argument(
    argument: &lamella_generics::TypeArg,
    program_tokens: &BTreeMap<String, Token>,
) -> Option<SigType> {
    match argument {
        lamella_generics::TypeArg::Named { name, value_type } => {
            let token = *program_tokens.get(name.as_ref())?;
            Some(if *value_type {
                SigType::ValueType(token)
            } else {
                SigType::Class(token)
            })
        }
        lamella_generics::TypeArg::Instance {
            definition,
            value_type,
            arguments,
        } => {
            let token = *program_tokens.get(definition.as_ref())?;
            let inner = arguments
                .iter()
                .map(|argument| expressible_argument(argument, program_tokens))
                .collect::<Option<Vec<_>>>()?;
            Some(SigType::GenericInst {
                definition: Box::new(if *value_type {
                    SigType::ValueType(token)
                } else {
                    SigType::Class(token)
                }),
                arguments: inner,
            })
        }
        lamella_generics::TypeArg::SzArray(element) => Some(SigType::SzArray(Box::new(
            expressible_argument(element, program_tokens)?,
        ))),
        other => payload_free_argument(other),
    }
}

/// The walk's decoded arguments as this assembly's signatures, when every one of them is a type the
/// element byte fully describes -- `int`, `string`, `object`, and arrays of those.
///
/// `None` the moment one is not. Kept as the leaf [`expressible_argument`] falls through to, so the
/// element-byte rule has one home rather than two.
#[cfg(feature = "generics")]
fn payload_free_arguments(arguments: &[lamella_generics::TypeArg]) -> Option<Vec<SigType>> {
    arguments.iter().map(payload_free_argument).collect()
}

/// One argument, per [`payload_free_arguments`].
#[cfg(feature = "generics")]
fn payload_free_argument(argument: &lamella_generics::TypeArg) -> Option<SigType> {
    match argument {
        lamella_generics::TypeArg::Primitive(byte) => {
            lamella_metadata::signature::payload_free_sig(*byte)
        }
        lamella_generics::TypeArg::SzArray(element) => {
            Some(SigType::SzArray(Box::new(payload_free_argument(element)?)))
        }
        _ => None,
    }
}

/// One closed instantiation to lower.
///
/// Every field is INPUT. In particular [`name`](Self::name) is supplied rather than computed --
/// see the module documentation for why this pass spells nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instantiation {
    /// The generic definition's full name, backtick arity included (`` Pair`2 ``).
    pub definition: String,
    /// The type arguments, in declaration order, as decoded signatures of the assembly being
    /// lowered. Tokens inside them are that assembly's.
    pub arguments: Vec<SigType>,
    /// The canonical spelling -- the instantiation's identity, and what
    /// [`Module::bind_type_full_name`] records.
    pub name: String,
}

/// One assembly a generic DEFINITION may live in, and the token space a body copied out of it
/// resolves against.
///
/// # Why this exists, and why it is not a rebase
///
/// A definition and the call sites that instantiate it are TWO assemblies the moment the definition
/// lives in a reference -- `List<int>` is written by the program and declared by the corlib. The
/// pass was written against one `assembly` playing both roles, so a definition that was not one of
/// the program's own `TypeDef`s was refused outright ([`Refusal::DefinitionNotHere`]).
///
/// The brief for this said the copied body's CORLIB tokens would have to be REBASED into the
/// program's module. They do not. [`Module::method_asm`] states what an assembly id is for: *"the
/// assembly id that owns `method` (the token space its CIL resolves against)"*. So a body copied out
/// of the corlib is emitted under the CORLIB's id, and every token the substitution does not
/// touch -- `ldstr "index"` into the corlib's user-string heap, the `MemberRef` to
/// `ArgumentOutOfRangeException::.ctor`, a `Field` naming the definition's own backing array -- keeps
/// exactly the binding the ordinary load already gave it. Nothing is rewritten. The tokens that DO
/// vary per instantiation were already being rewritten into [`SYNTHETIC_TABLE`] and bound
/// explicitly, and that mechanism does not care which assembly it mints rows in.
///
/// `MethodId` and `TypeId` are module-global, so a call site in the PROGRAM binding to a body
/// emitted under the corlib's id is well formed: phase 5 binds the program's tokens to ids, and an
/// id carries its own token space with it.
///
/// **A TOKEN INSIDE A TYPE ARGUMENT IS CARRIED TOO.** An instantiation's identity is a canonical
/// NAME, and a name has no world, so the two sides never have to be comparable in one.
#[derive(Clone)]
pub(crate) struct DefinitionSource<'pe> {
    /// The assembly itself: read for the definition's `TypeDef`, its fields, its interfaces and its
    /// method bodies.
    pub assembly: Assembly<'pe>,
    /// Its assembly id -- the token space a body copied out of it is emitted under.
    pub asm: u8,
    /// The module type index this assembly's `TypeDef` row 1 landed at, where its types were loaded
    /// contiguously. `None` on a tier that materializes types on demand, where no such offset
    /// exists and a `TypeDef` token has to be resolved through the module's own map instead.
    pub type_offset: Option<usize>,
}

/// Why one instantiation, or one token inside one, could not be lowered.
///
/// **A REFUSAL LEAVES THE `UnloweredGeneric` MARK IN PLACE**, so the bake still refuses the
/// program by name. Lowering PART of a program and clearing the marks anyway is the one outcome
/// this whole mechanism exists to prevent: an image that bakes, runs, and is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The definition named by the set is not a `TypeDef` of ANY assembly this load can see. The
    /// set is built from one assembly's roots and walked across all of them, so this is a real
    /// possibility and not a malformed input -- but a body cannot be duplicated out of metadata
    /// that is not here.
    ///
    /// Which assemblies "here" means is [`DefinitionSource`]'s job: the program and every reference
    /// it was loaded against, so a definition in the corlib is found rather than refused.
    DefinitionNotHere {
        /// The definition's full name.
        definition: String,
    },
    /// The definition declares a STATIC field. Each instantiation owns its own copy of a generic
    /// type's statics (ECMA-335 II.9.7 -- `Counter<int>.Total` and `Counter<string>.Total` are two
    /// storage cells), and this pass emits one shared static slot set. Refused rather than
    /// silently sharing, because sharing is observable and wrong.
    StaticFieldNotSeparated {
        /// The instantiation that wanted it.
        instantiation: String,
        /// The field's name.
        field: String,
    },
    /// A body names `!!n` -- a METHOD type parameter. Its argument comes from a `MethodSpec` at the
    /// CALL SITE rather than from the enclosing type's instantiation, so substituting the type's
    /// arguments into it would leave a type that LOOKS closed and is not.
    MethodTypeParameter {
        /// The instantiation whose body named it.
        instantiation: String,
        /// The token that carried it.
        token: u32,
    },
    /// A body names an instantiation that is not in the set it was given. The set is the
    /// collector's output, so this is the collector reporting less than the bodies require --
    /// refused rather than lowered-around, because the call would otherwise point at nothing.
    InstantiationNotInSet {
        /// The instantiation whose body named it.
        instantiation: String,
        /// The definition that was named.
        wanted: String,
    },
    /// An open token substituted to something this pass cannot bind: a member that the definition
    /// does not declare, a type argument with no resolvable identity, or a shape not handled yet.
    UnboundAfterSubstitution {
        /// The instantiation whose body named it.
        instantiation: String,
        /// The original token, before rewriting.
        token: u32,
    },
    /// Two of the definition's methods spell the SAME dispatch key once the arguments are
    /// substituted in: `void M(K)` and `void M(int)` are distinct on `Pair<K,V>` and identical on
    /// `Pair<int,string>`.
    ///
    /// Substitution is still the right side to be on -- an INTERFACE call's key is closed by
    /// construction, so an unsubstituted map cannot dispatch one at all. This is the residue, and
    /// it is refused rather than resolved by insertion order, which would silently pick whichever
    /// method the metadata happened to declare second.
    SubstitutedKeyCollision {
        /// The instantiation whose map they collided in.
        instantiation: String,
        /// The key they both spell.
        key: String,
    },
    /// A method body did not read. Reported rather than skipped: a body that silently ceases to
    /// exist is worse than one that loudly fails to load.
    BodyUnreadable {
        /// The instantiation whose method it was.
        instantiation: String,
        /// The method's name.
        method: String,
    },
    /// A `MethodSpec`'s own type arguments still mention a type parameter, so the
    /// (method, arguments) pair is not closed.
    ///
    /// **THIS IS THE SEQUENCING CONSTRAINT, AS A REFUSAL.** A generic call inside a generic type's
    /// body names arguments like `!0`, which only the enclosing instantiation can close. The method
    /// axis runs after the type axis for exactly that reason -- but a pair can still arrive open
    /// (the enclosing type was refused, or the call sits in an ordinary body the type axis never
    /// visits), and lowering one would emit a body for an instantiation that does not exist.
    OpenMethodInstantiation {
        /// The `MethodSpec` token.
        token: u32,
    },
    /// A `MethodSpec` names a generic method with no `MethodDef` in this assembly -- an IMPORTED
    /// one, or one reached through a `TypeSpec` parent (a generic method on an instantiated generic
    /// type). There is no body here to duplicate.
    ///
    /// The call site keeps its `UnloweredGeneric` mark, so the bake refuses it by name. **It is NOT
    /// bound to the definition instead.** Falling back from a generic call site to the definition's
    /// own token produces a call that BINDS -- to the open method, with `!!0` never substituted --
    /// and raises nothing, which is a wrong program rather than a degraded one. The emitter refuses
    /// the mirror of this by construction on its own side.
    GenericMethodNotHere {
        /// The `MethodSpec` token.
        token: u32,
    },
    /// A generic call inside a COPIED body that this pass could not turn into a pair.
    ///
    /// The common case is handled: a generic call inside a generic TYPE's body has its arguments
    /// closed by the enclosing instantiation and becomes an ordinary pair. This is the residue --
    /// a call whose method has no `MethodDef` here to duplicate, or one inside a lowered METHOD's
    /// body, where the enclosing PAIR's own arguments would have to be threaded in as well.
    ///
    /// It names the enclosing pair as well as the token, because a generic call site's own text
    /// appears once in the source and can be reached from several instantiations -- "which body was
    /// being copied" is the half that says which one failed.
    NestedGenericMethodCall {
        /// The pair whose body named it.
        owner: String,
        /// The nested `MethodSpec` token.
        token: u32,
    },
    /// A body copied out of a REFERENCE names a generic METHOD.
    ///
    /// Distinct from [`NestedGenericMethodCall`](Self::NestedGenericMethodCall), which is about the
    /// pass being unable to CLOSE a nested call's arguments. This one is closed and still refused,
    /// for a reason about tables rather than about types: the site is recorded for the method axis,
    /// and the method axis walks the PROGRAM's `MethodSpec` table. A token written in the corlib's
    /// table is not a row of the program's, so looking it up there does not miss -- it names
    /// whichever row of that number the program happens to have.
    MethodSpecFromAnotherAssembly {
        /// The instantiation whose copied body named it.
        owner: String,
        /// The `MethodSpec` token, as the definition's assembly wrote it.
        token: u32,
    },
    /// Two DISTINCT instantiations hash to the same EXCEPTION TAG, so a `catch` of one would catch
    /// the other.
    ///
    /// **The AOT tier refuses the same shape and the rule is taken from there**
    /// (`lamella_generics::Refusal::HandleCollision`): two types under one tag is the precise
    /// failure the by-name identity exists to prevent, and a build that cannot name a type uniquely
    /// must stop rather than pick one. **That check does not cover this one**, which is why this
    /// variant exists at all: its handle is 24 bits of the hash, this tier's exception tag is 32
    /// with the high bit forced, so the two collide on different inputs. The criterion transfers;
    /// the computation does not.
    ///
    /// Rarer than theirs and NOT absent -- which is the more dangerous of the two, because a
    /// collision nobody's fixture reproduces is one nobody believes in.
    ExceptionTagCollision {
        /// One instantiation's canonical name.
        first: String,
        /// The other's.
        second: String,
        /// The tag they both mint.
        tag: u32,
    },
    /// The generic method is VIRTUAL -- which body runs is a property of the receiver at run time
    /// and not of the token -- and this pass could not give the site a receiver-chosen body.
    ///
    /// **THIS IS THE ONE REFUSAL HERE WHOSE ABSENCE WOULD BE SILENT.** Every other shape this pass
    /// declines leaves a call site unbound and the bake names it. Binding a virtual generic call to
    /// the body its TOKEN names is a bind that SUCCEEDS: the program loads, bakes clean and runs,
    /// and calls the base's `M<int>` on an object whose type overrides it. Measured by removing the
    /// check -- zero violations and the wrong answer, with nothing to look for at bake time.
    ///
    /// # One mechanism per name
    ///
    /// The five variants below are that refusal, separated. A refusal name is read as the CAUSE by
    /// everyone downstream, so a single name spanning several causes teaches each reader the wrong
    /// one and hides the day any one of them is fixed. Each of the five moves independently when its
    /// own mechanism lands.
    ///
    /// This variant is the first of them: a virtual generic call reached from INSIDE a duplicated
    /// body (a deferred method site), which would need the enclosing pair's type arguments threaded
    /// through the receiver as well as through the call.
    VirtualGenericInDuplicatedBody {
        /// The `MethodSpec` token.
        token: u32,
        /// The method's name.
        method: String,
    },
    /// The declaring type's name, the method's signature, or the pair label did not decode, so there
    /// is nothing to plan a body against. A metadata-shape refusal rather than a capability one: it
    /// says the rows could not be read, not that the shape is unsupported.
    VirtualGenericDeclarationUnreadable {
        /// The `MethodSpec` token.
        token: u32,
        /// The method's name.
        method: String,
    },
    /// The override closure reached a type this program does not itself declare -- either the walk
    /// cannot enter its assembly, or no program-side pair could be located for it. Its body would
    /// have to be copied out of that assembly and dispatched from here, which this pass does not do.
    VirtualGenericOverrideNotInThisProgram {
        /// The `MethodSpec` token.
        token: u32,
        /// The method's name.
        method: String,
    },
    /// Every override was planned but at least one body did not come back from emission, so the site
    /// would dispatch a derived receiver to a base body. **A partial success is worse than none**,
    /// which is why the whole site is refused rather than the missing arm alone.
    VirtualGenericBodyNotEmitted {
        /// The `MethodSpec` token.
        token: u32,
        /// The method's name.
        method: String,
    },
    /// A lowered body that no loaded type dispatches to: the hierarchy this pass read and the one
    /// the ordinary load built have diverged.
    ///
    /// **This one reports a DEFECT rather than a limit.** The other four say a shape is not lowered
    /// here; this one says two readings of the same hierarchy disagree, so seeing it means something
    /// is wrong rather than merely unsupported. Treat it as a bug report, not a capability gap.
    VirtualGenericDispatchDiverged {
        /// The `MethodSpec` token.
        token: u32,
        /// The method's name.
        method: String,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::DefinitionNotHere { definition } => {
                write!(formatter, "generic definition `{definition}` is not defined in this assembly, so no body can be duplicated from it")
            }
            Refusal::StaticFieldNotSeparated { instantiation, field } => {
                write!(formatter, "`{instantiation}` declares the static field `{field}`, and each instantiation owns its own copy of a generic type's statics -- not separated yet")
            }
            Refusal::MethodTypeParameter { instantiation, token } => {
                write!(formatter, "`{instantiation}` names a method type parameter at token 0x{token:08X}; its argument comes from the call site, not from this instantiation")
            }
            Refusal::InstantiationNotInSet { instantiation, wanted } => {
                write!(formatter, "`{instantiation}` names an instantiation of `{wanted}` that the instantiation set does not contain")
            }
            Refusal::UnboundAfterSubstitution { instantiation, token } => {
                write!(formatter, "`{instantiation}` names token 0x{token:08X}, which substitution did not resolve to anything bindable")
            }
            Refusal::SubstitutedKeyCollision { instantiation, key } => {
                write!(formatter, "two of `{instantiation}`'s methods spell the same dispatch key `{key}` once its type arguments are substituted in")
            }
            Refusal::BodyUnreadable { instantiation, method } => {
                write!(formatter, "`{instantiation}`'s method `{method}` has a body that did not read")
            }
            Refusal::OpenMethodInstantiation { token } => {
                write!(formatter, "the generic call at token 0x{token:08X} names type arguments that still mention a type parameter, so the (method, arguments) pair is not closed")
            }
            Refusal::GenericMethodNotHere { token } => {
                write!(formatter, "the generic call at token 0x{token:08X} names a method this assembly does not define, so there is no body to duplicate for it")
            }
            Refusal::NestedGenericMethodCall { owner, token } => {
                write!(formatter, "`{owner}`'s body calls another generic method at token 0x{token:08X}, which needs its own lowered pair")
            }
            Refusal::MethodSpecFromAnotherAssembly { owner, token } => {
                write!(formatter, "`{owner}`'s body was copied out of a reference and calls a generic method at token 0x{token:08X}, which is a row of that assembly's table and not of the program's")
            }
            Refusal::ExceptionTagCollision { first, second, tag } => {
                write!(formatter, "`{first}` and `{second}` both mint exception tag 0x{tag:08X}, so a `catch` of either would catch the other")
            }
            Refusal::VirtualGenericInDuplicatedBody { token, method } => {
                write!(formatter, "the generic call at token 0x{token:08X} names the VIRTUAL method `{method}` from inside a duplicated body, so the enclosing pair's type arguments would have to be threaded through the receiver as well")
            }
            Refusal::VirtualGenericDeclarationUnreadable { token, method } => {
                write!(formatter, "the VIRTUAL generic method `{method}` at token 0x{token:08X} has a declaring type, signature or pair label that did not decode, so there is nothing to plan a body against")
            }
            Refusal::VirtualGenericOverrideNotInThisProgram { token, method } => {
                write!(formatter, "the VIRTUAL generic method `{method}` at token 0x{token:08X} is overridden in an assembly this program does not declare, so its body cannot be copied out and dispatched from here")
            }
            Refusal::VirtualGenericBodyNotEmitted { token, method } => {
                write!(formatter, "the VIRTUAL generic method `{method}` at token 0x{token:08X} had an override whose body did not emit, and a site that dispatched a derived receiver to a base body would be worse than a refusal")
            }
            Refusal::VirtualGenericDispatchDiverged { token, method } => {
                write!(formatter, "the VIRTUAL generic method `{method}` at token 0x{token:08X} lowered a body no loaded type dispatches to, so the override closure and the load's own hierarchy disagree -- a defect rather than a limit")
            }
        }
    }
}

/// What one run of the pass produced.
pub struct Lowering {
    /// Each instantiation that was lowered, and the type identity it was lowered to.
    pub types: Vec<(String, TypeId)>,
    /// Everything it refused. Empty means every instantiation in the set now has a type identity
    /// and every call site that reaches one is bound.
    pub refusals: Vec<Refusal>,
}

/// One instantiation, after its type identity exists but before its bodies do.
struct Emitted {
    type_id: TypeId,
    /// Which [`DefinitionSource`] declares the definition -- an index into the caller's list.
    source: usize,
    /// The definition's `TypeDef` row (1-based) within [`source`](Self::source)'s assembly.
    def_row: u32,
    /// The slot each of the definition's own instance fields lands in, by declaration order.
    own_field_slots: Vec<u32>,
    /// The STATIC storage slot each of the definition's own non-literal static fields lands in, by
    /// declaration order. Separate from `own_field_slots` because they index different things: an
    /// instance field's slot is an offset within this type's own value layout, a static's is an
    /// index into the module's one flat static array (II.9.7 -- each instantiation owns its own).
    own_static_slots: Vec<usize>,
    /// The `MethodId` of each of the definition's methods, by declaration order. `None` where the
    /// definition's method has no body to duplicate (abstract, or runtime-supplied).
    methods: Vec<Option<MethodId>>,
    /// Whether the definition is a STRUCT, so this instantiation is a value type.
    ///
    /// # Why it is carried rather than re-derived where it is needed
    ///
    /// Three things downstream ask it -- the type's own flag, and the `newobj` mark at each of the
    /// two call-site binders -- and each of them would otherwise reach back through
    /// `(source, def_row)` to the `TypeDef` and re-apply the rule. **A rule with several
    /// implementations gains a new case in none of them**, and this one has a case already: the two
    /// CLI bases that extend a value type and are themselves references.
    is_value_type: bool,
}

/// A definition's method as this pass needs to see it.
struct DefMethod<'pe> {
    name: String,
    /// The parameter types AS DECLARED -- still mentioning `!n`, which is also the form the call
    /// site's `MemberRef` carries. Matching happens on this form; the DISPATCH KEY is built from
    /// its substitution.
    params: Vec<SigType>,
    arg_count: u16,
    /// How many type parameters the method itself declares, or 0. Part of the dispatch key
    /// (II.23.2.1 makes it binding-significant), so it has to survive substitution alongside the
    /// parameters -- substituting a TYPE argument does not change a METHOD's own arity.
    generic_arity: u32,
    is_static: bool,
    is_virtual: bool,
    newslot: bool,
    /// The raw method body bytes, or `None` for a bodyless method.
    raw: Option<&'pe [u8]>,
}

/// Lowers every instantiation in `instantiations` into its own type identity in `module`.
///
/// Returns what it lowered and what it refused; a refusal leaves the corresponding
/// `UnloweredGeneric` marks in place, so the bake still refuses.
pub(crate) fn monomorphize<'pe>(
    module: &mut Module,
    program: &Assembly<'pe>,
    program_asm: u8,
    sources: &[DefinitionSource<'pe>],
    type_index: &TypeNameIndex,
    field_index: &FieldNameIndex,
    materialize: super::CilMaterializer<'pe>,
    instantiations: &[Instantiation],
) -> Lowering {
    let mut lowering = Lowering {
        types: Vec::new(),
        refusals: Vec::new(),
    };

    let mut definition_rows: BTreeMap<String, (usize, u32)> = BTreeMap::new();
    for (position, source) in sources.iter().enumerate() {
        let mut row = 0u32;
        for type_def in source.assembly.type_defs() {
            row += 1;
            if let Some(name) = definition_key(&source.assembly, type_def.token()) {
                definition_rows.entry(name).or_insert((position, row));
            }
        }
    }

    let mut emitted: Vec<(usize, Emitted)> = Vec::new();
    for (index, want) in instantiations.iter().enumerate() {
        let Some(&(source, def_row)) = definition_rows.get(&want.definition) else {
            lowering.refusals.push(Refusal::DefinitionNotHere {
                definition: want.definition.clone(),
            });
            continue;
        };
        let type_id = module.add_type(Vec::new());
        module.bind_type_full_name(type_id, want.name.clone());
        lowering.types.push((want.name.clone(), type_id));
        let is_value_type = sources[source]
            .assembly
            .type_def(def_row)
            .is_some_and(|type_def| {
                type_def.is_value_type() && !is_special_reference_base(type_def.name())
            });
        module.set_type_is_value_type(type_id, is_value_type);
        bind_nullable_underlying(module, program, type_index, want, type_id, source);
        emitted.push((
            index,
            Emitted {
                type_id,
                source,
                def_row,
                own_field_slots: Vec::new(),
                own_static_slots: Vec::new(),
                methods: Vec::new(),
                is_value_type,
            },
        ));
    }

    for (index, entry) in &mut emitted {
        let want = &instantiations[*index];
        let source = &sources[entry.source];
        let assembly = &source.assembly;
        let Some(type_def) = assembly.type_def(entry.def_row) else {
            continue;
        };
        let base = base_type_of(
            module,
            assembly,
            source.asm,
            source.type_offset,
            type_index,
            type_def.extends(),
        );
        module.set_type_base(entry.type_id, base);
        let mut defaults = base
            .and_then(|base| module.type_field_defaults(base))
            .unwrap_or_default();
        let mut refused = false;
        let static_start = module.static_field_count();
        for field in type_def.fields() {
            if field.is_static() {
                if !field.is_literal() {
                    let Some(substituted) = field
                        .signature()
                        .and_then(|sig| substitute(&sig, &want.arguments))
                    else {
                        lowering.refusals.push(Refusal::UnboundAfterSubstitution {
                            instantiation: want.name.clone(),
                            token: 0,
                        });
                        refused = true;
                        continue;
                    };
                    let zero = default_field_value_substituted(
                        module,
                        assembly,
                        source.asm,
                        source.type_offset,
                        type_index,
                        &substituted,
                        &field_index.enum_zeros,
                    );
                    entry.own_static_slots.push(module.reserve_static_slot(zero));
                }
                continue;
            }
            let Some(substituted) = field
                .signature()
                .and_then(|sig| substitute(&sig, &want.arguments))
            else {
                lowering.refusals.push(Refusal::UnboundAfterSubstitution {
                    instantiation: want.name.clone(),
                    token: 0,
                });
                refused = true;
                continue;
            };
            entry.own_field_slots.push(defaults.len() as u32);
            defaults.push(default_field_value_substituted(
                module,
                assembly,
                source.asm,
                source.type_offset,
                type_index,
                &substituted,
                &field_index.enum_zeros,
            ));
        }
        module.bind_static_slot_range(
            u32::try_from(static_start).unwrap_or(u32::MAX),
            u32::try_from(module.static_field_count()).unwrap_or(u32::MAX),
            entry.type_id,
        );
        if refused {
            continue;
        }
        module.set_type_field_defaults(entry.type_id, defaults);
    }

    for (index, entry) in &emitted {
        let want = &instantiations[*index];
        let source = &sources[entry.source];
        let assembly = &source.assembly;
        let asm = source.asm;
        let Some(type_def) = assembly.type_def(entry.def_row) else {
            continue;
        };
        let mut resolved = Vec::new();
        for token in type_def.interfaces() {
            let interface_id = if token.table() == TYPE_SPEC {
                assembly
                    .type_spec_signature(token)
                    .and_then(|sig| substitute(&sig, &want.arguments))
                    .and_then(|closed| match closed {
                        SigType::GenericInst {
                            definition,
                            arguments,
                        } => {
                            let definition_token = match definition.as_ref() {
                                SigType::Class(token) | SigType::ValueType(token) => *token,
                                _ => return None,
                            };
                            let name = definition_key(assembly, definition_token)?;
                            let target =
                                find_instantiation(instantiations, &emitted, &name, &arguments)?;
                            Some(emitted[target].1.type_id)
                        }
                        _ => None,
                    })
            } else {
                module.type_id_of(asm, token).or_else(|| {
                    assembly
                        .type_token_name(token)
                        .and_then(|name| type_index.get(&type_name_key(name)).copied())
                })
            };
            if let Some(interface_id) = interface_id {
                resolved.push(interface_id);
            }
        }
        if !resolved.is_empty() {
            module.set_type_interfaces(entry.type_id, resolved);
        }
    }

    let mut definition_methods: BTreeMap<(usize, u32), Vec<DefMethod<'pe>>> = BTreeMap::new();
    for (_, entry) in &emitted {
        definition_methods
            .entry((entry.source, entry.def_row))
            .or_insert_with(|| {
                read_definition_methods(&sources[entry.source].assembly, entry.def_row)
            });
    }

    let mut deferred: Vec<DeferredMethodSite> = Vec::new();
    let mut next_synthetic_row: u32 = 1;

    let mut plans: Vec<Vec<Vec<OpenSite>>> = Vec::with_capacity(emitted.len());
    for position in 0..emitted.len() {
        let (index, source_index, def_row, type_id) = {
            let (index, entry) = &emitted[position];
            (*index, entry.source, entry.def_row, entry.type_id)
        };
        let assembly = &sources[source_index].assembly;
        let asm = sources[source_index].asm;
        let want = &instantiations[index];
        let methods = definition_methods
            .get(&(source_index, def_row))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut ids: Vec<Option<MethodId>> = Vec::with_capacity(methods.len());
        let mut method_plans: Vec<Vec<OpenSite>> = Vec::with_capacity(methods.len());
        for method in methods {
            let Some(raw) = method.raw else {
                ids.push(None);
                method_plans.push(Vec::new());
                continue;
            };
            let plan = match plan_rewrite(assembly, raw, Some(def_row)) {
                Ok(plan) => plan,
                Err(()) => {
                    lowering.refusals.push(Refusal::BodyUnreadable {
                        instantiation: want.name.clone(),
                        method: method.name.clone(),
                    });
                    ids.push(None);
                    method_plans.push(Vec::new());
                    continue;
                }
            };
            let id = module.add_method(asm, materialize(raw), method.arg_count);
            module.set_method_type(id, type_id);
            if method.name == ".cctor" {
                module.add_static_ctor(id);
            }
            #[cfg(feature = "debug-names")]
            module.set_method_debug(
                id,
                alloc::format!("{}.{}", want.name, method.name),
                Vec::new(),
            );
            ids.push(Some(id));
            method_plans.push(plan);
        }
        emitted[position].1.methods = ids;
        plans.push(method_plans);
    }

    let mut withdrawn: Vec<(usize, usize)> = Vec::new();
    for position in 0..emitted.len() {
        let (index, source_index, def_row, type_id, own_statics, own_instance) = {
            let (index, entry) = &emitted[position];
            (
                *index,
                entry.source,
                entry.def_row,
                entry.type_id,
                entry.own_static_slots.clone(),
                entry.own_field_slots.clone(),
            )
        };
        let assembly = &sources[source_index].assembly;
        let asm = sources[source_index].asm;
        let type_offset = sources[source_index].type_offset;
        let want = &instantiations[index];
        let owner_statics = Some(OwnerLayout {
            def_row,
            statics: own_statics.as_slice(),
            instance: own_instance.as_slice(),
            type_id,
        });
        let methods = definition_methods
            .get(&(source_index, def_row))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for (method_position, method) in methods.iter().enumerate() {
            let plan = &plans[position][method_position];
            if plan.is_empty() {
                continue;
            }
            let (Some(id), Some(raw)) = (
                emitted[position].1.methods[method_position],
                method.raw,
            ) else {
                continue;
            };
            let mut bytes = raw.to_vec();
            let mut body_refused = false;
            for site in plan {
                let synthetic = Token::new(SYNTHETIC_TABLE, next_synthetic_row);
                next_synthetic_row += 1;
                match bind_open_token(
                    module,
                    assembly,
                    asm,
                    type_offset,
                    sources,
                    source_index,
                    type_index,
                    field_index,
                    instantiations,
                    &emitted,
                    &definition_methods,
                    &want.name,
                    &want.arguments,
                    &[],
                    owner_statics,
                    site,
                    synthetic,
                    &mut deferred,
                ) {
                    Ok(()) => {
                        let at = site.operand_at;
                        bytes[at..at + 4].copy_from_slice(&synthetic.0.to_le_bytes());
                    }
                    Err(refusal) => {
                        lowering.refusals.push(refusal);
                        body_refused = true;
                    }
                }
            }
            if body_refused {
                withdrawn.push((position, method_position));
                continue;
            }
            module.set_managed_body(id, owned_cil(bytes), method.arg_count);
        }
    }
    for (position, method_position) in withdrawn {
        emitted[position].1.methods[method_position] = None;
    }

    for (index, entry) in &emitted {
        let want = &instantiations[*index];
        let assembly = &sources[entry.source].assembly;
        let methods = definition_methods
            .get(&(entry.source, entry.def_row))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut vtable: Vec<(String, MethodId)> = module
            .type_base(entry.type_id)
            .and_then(|base| module.vtable_slot_keys(base))
            .unwrap_or_default();
        let mut virtuals: BTreeMap<String, MethodId> = vtable.iter().cloned().collect();
        let mut nonvirtuals: BTreeMap<String, MethodId> = BTreeMap::new();
        let mut slots: Vec<(MethodId, u32)> = Vec::new();
        for (position, method) in methods.iter().enumerate() {
            let Some(Some(id)) = entry.methods.get(position).copied() else {
                continue;
            };
            let Some(key) = substituted_sig_key(
                assembly,
                &method.name,
                &method.params,
                &want.arguments,
                method.generic_arity,
            )
            else {
                continue;
            };
            if method.is_virtual {
                let overridden = (!method.newslot)
                    .then(|| vtable.iter().position(|(slot, _)| *slot == key))
                    .flatten();
                if overridden.is_none() && virtuals.contains_key(&key) {
                    lowering.refusals.push(Refusal::SubstitutedKeyCollision {
                        instantiation: want.name.clone(),
                        key: key.clone(),
                    });
                    continue;
                }
                let slot = match overridden {
                    Some(slot) => {
                        vtable[slot].1 = id;
                        slot as u32
                    }
                    None => {
                        vtable.push((key.clone(), id));
                        (vtable.len() - 1) as u32
                    }
                };
                slots.push((id, slot));
                virtuals.insert(key, id);
            } else if !method.is_static && method.name != ".ctor" {
                if nonvirtuals.contains_key(&key) {
                    lowering.refusals.push(Refusal::SubstitutedKeyCollision {
                        instantiation: want.name.clone(),
                        key: key.clone(),
                    });
                    continue;
                }
                nonvirtuals.insert(key, id);
            }
        }
        module.set_vtable_slot_keys(entry.type_id, vtable.clone());
        if !vtable.is_empty() {
            module.set_vtable(entry.type_id, vtable.iter().map(|(_, id)| *id).collect());
        }
        if !virtuals.is_empty() {
            module.set_sig_methods(entry.type_id, virtuals);
        }
        if !nonvirtuals.is_empty() {
            module.set_sig_methods_nonvirtual(entry.type_id, nonvirtuals);
        }
        for (id, slot) in slots {
            module.bind_method_slot(id, slot);
        }
    }

    #[cfg(feature = "exceptions")]
    if let Some(refusal) = exception_tag_collision(instantiations, &emitted) {
        lowering.refusals.push(refusal);
        return lowering;
    }
    bind_closed_type_specs(module, program, program_asm, instantiations, &emitted);
    bind_closed_member_refs(
        module,
        program,
        program_asm,
        sources,
        instantiations,
        &emitted,
        &definition_methods,
        &mut lowering,
    );

    lower_method_pairs(
        module,
        program,
        program_asm,
        sources[PROGRAM_SOURCE].type_offset,
        sources,
        type_index,
        field_index,
        materialize,
        instantiations,
        &emitted,
        &definition_methods,
        &deferred,
        &mut next_synthetic_row,
        &mut lowering,
    );

    lowering
}

/// One (generic method, CALL-SITE type arguments) pair -- the unit the method axis emits a body for.
///
/// The type axis keys on a TYPE's instantiation and emits one identity per instantiation. This
/// cannot: a `MethodSpec`'s arguments come from the call site, so ONE enclosing type can call ONE
/// definition at several of them, and the pair is the smallest thing that has its own body.
#[cfg(feature = "generics")]
struct MethodPair {
    /// The definition's `MethodDef` row (1-based).
    def_row: u32,
    /// The call-site type arguments, decoded from the `MethodSpec`'s instantiation blob.
    arguments: Vec<SigType>,
    /// The display label -- `Cell.Zero<System.Int32>`. See [`method_pair_label`].
    label: String,
    /// Every `MethodSpec` token that names this pair. csc normally writes one row per shape, but
    /// two rows spelling the same pair must reach one body rather than two.
    tokens: Vec<Token>,
}

/// The pair's DISPLAY label.
///
/// **THIS IS NOT AN IDENTITY, AND THE DIFFERENCE IS THE WHOLE REASON THE MODULE SPELLS NOTHING.** A
/// type instantiation's canonical name IS its identity -- the AOT tier mints a frozen tag from
/// it, so two spellings agreeing on `` Pair`2[int,string] `` and diverging on the first nested
/// argument produce one type that exists twice. This label is read by a refusal message and a debug
/// name and by nothing else.
///
/// It still goes through `lamella_generics::spell_sig` rather than a local formatter, so that the
/// day the AOT tier needs a method-pair SYMBOL the arguments are already spelled its way and the
/// only new decision is how to join them. A `None` argument refuses the pair instead of inventing a
/// name for it.
#[cfg(feature = "generics")]
fn method_pair_label(
    assembly: &Assembly<'_>,
    declaring: &str,
    method: &str,
    arguments: &[SigType],
) -> Option<String> {
    let mut label = alloc::format!("{declaring}.{method}<");
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            label.push(',');
        }
        label.push_str(&lamella_generics::spell_sig(assembly, argument)?);
    }
    label.push('>');
    Some(label)
}

/// Without the `generics` capability the method axis does not run at all, so every call site keeps
/// the `UnloweredGeneric` mark `bind_generic_calls` wrote and the bake refuses the program by name.
/// That is the state this whole mechanism degrades to, and it is the one the feature promises.
#[cfg(not(feature = "generics"))]
#[allow(clippy::too_many_arguments)]
fn lower_method_pairs<'pe>(
    _module: &mut Module,
    _assembly: &Assembly<'pe>,
    _asm: u8,
    _type_offset: Option<usize>,
    _sources: &[DefinitionSource<'pe>],
    _type_index: &TypeNameIndex,
    _field_index: &FieldNameIndex,
    _materialize: super::CilMaterializer<'pe>,
    _instantiations: &[Instantiation],
    _emitted: &[(usize, Emitted)],
    _definition_methods: &BTreeMap<(usize, u32), Vec<DefMethod<'pe>>>,
    _deferred: &[DeferredMethodSite],
    _next_synthetic_row: &mut u32,
    _lowering: &mut Lowering,
) {
}

/// Emits one body per (generic method, call-site type arguments) pair and binds the `MethodSpec`
/// token that names each, withdrawing its `UnloweredGeneric` mark only after the bind.
///
/// # What it walks, and why it is not a second collector
///
/// The `MethodSpec` TABLE, row by row. There is no closure to walk: a row exists because a call
/// site named it, the table is finite, and a generic method that calls itself at a growing
/// instantiation cannot enlarge it -- so the growth-on-a-cycle refusal the TYPE set needs has
/// nothing to refuse here. Both halves of each row come from `lamella_metadata::reader`'s existing
/// decoders (`method_spec_method` and `method_spec_instantiation`), whose own documentation names
/// the trap this pass would otherwise fall into: a consumer with only the first binds to the open
/// definition and leaves `!!0` unresolved.
///
/// # A virtual row takes the other arm, and its token is never bound
///
/// For every non-virtual pair the shape is one site, one body, one `bind_token`. A VIRTUAL generic
/// method is one site and N bodies, with which one runs decided by the RECEIVER -- III.4.2 says a
/// `callvirt` whose token is a `methodspec` chooses its body by the receiver's exact type and not by
/// the token. Binding it would be a bind that SUCCEEDS and answers the base's declaration.
///
/// So those rows are collected and handed to [`expand_virtual_rows`], which closes the set over the
/// OVERRIDE relation and plans one body per member of it. They reach the same emission loop below --
/// the bodies are ordinary duplicated bodies -- and differ in what happens after: no token bind, no
/// vtable slot, and instead a per-instantiation entry in the dispatch map of every type that must
/// reach each one ([`dispatch_virtual_sites`]).
///
/// **A vtable slot is not available to this, and that is structural rather than a preference.** A
/// frozen table cannot grow (`Module::vtable_entry` answers `None` past the frozen count, with no
/// fallback) and the REPL mutates a live `Module`, so a scheme that adds one slot per argument list
/// cannot be spelled here at all. The signature map can take entries at any time, which is why the
/// arm is the signature-keyed one.
///
/// # What it deliberately leaves alone
///
/// A `MethodSpec` that already RESOLVES was bound by `bind_generic_calls` to a BCL intrinsic that
/// never sees `T` (`Array.Empty<T>`), and re-binding it to a duplicated body would be wrong twice
/// over -- there is no body, and the intrinsic is the right answer. Asking the module whether the
/// token resolves is what keeps this pass from having to know which names those are.
///
/// # Off without the `generics` capability, and that has to be here rather than upstream
///
/// `monomorphize` is called UNCONDITIONALLY; what the `generics` feature gates is the instantiation
/// SET, which becomes empty. That is enough to switch the TYPE axis off, because an empty set is a
/// pass with nothing to do -- but the method axis reads the `MethodSpec` table directly and would
/// carry on lowering, which is precisely the "a build without this capability REFUSES a generic
/// program rather than mis-running one" contract that feature's own documentation states.
#[cfg(feature = "generics")]
#[allow(clippy::too_many_arguments)]
fn lower_method_pairs<'pe>(
    module: &mut Module,
    assembly: &Assembly<'pe>,
    asm: u8,
    type_offset: Option<usize>,
    sources: &[DefinitionSource<'pe>],
    type_index: &TypeNameIndex,
    field_index: &FieldNameIndex,
    materialize: super::CilMaterializer<'pe>,
    instantiations: &[Instantiation],
    emitted: &[(usize, Emitted)],
    definition_methods: &BTreeMap<(usize, u32), Vec<DefMethod<'pe>>>,
    deferred: &[DeferredMethodSite],
    next_synthetic_row: &mut u32,
    lowering: &mut Lowering,
) {
    let mut declaring_rows: Option<BTreeMap<u32, u32>> = None;

    let mut pairs: Vec<MethodPair> = Vec::new();
    let mut virtual_rows: Vec<(Token, u32, Vec<SigType>)> = Vec::new();
    let mut virtual_sites: Vec<VirtualSite> = Vec::new();
    for row in 1..=u32::from(u16::MAX) {
        let token = Token::new(METHOD_SPEC, row);
        let Some(definition) = assembly.method_spec_method(token) else {
            break;
        };
        if module.resolve(asm, token).is_some() {
            continue;
        }
        if definition.table() != METHOD_DEF {
            lowering
                .refusals
                .push(Refusal::GenericMethodNotHere { token: token.0 });
            continue;
        }
        let Some(method) = assembly.method(definition.row()) else {
            lowering
                .refusals
                .push(Refusal::GenericMethodNotHere { token: token.0 });
            continue;
        };
        let Some(arguments) = assembly.method_spec_instantiation(token) else {
            lowering
                .refusals
                .push(Refusal::GenericMethodNotHere { token: token.0 });
            continue;
        };
        if arguments.iter().any(mentions_parameter) {
            lowering
                .refusals
                .push(Refusal::OpenMethodInstantiation { token: token.0 });
            continue;
        }
        if method.flags() & super::METHOD_VIRTUAL != 0 {
            virtual_rows.push((token, definition.row(), arguments));
            continue;
        }
        if let Some(existing) = pairs
            .iter_mut()
            .find(|pair| pair.def_row == definition.row() && pair.arguments == arguments)
        {
            existing.tokens.push(token);
            continue;
        }
        let rows = declaring_rows.get_or_insert_with(|| declaring_row_map(assembly));
        let declaring = rows
            .get(&definition.row())
            .and_then(|&type_row| definition_key(assembly, Token::new(TYPE_DEF, type_row)));
        let (Some(declaring), Some(name)) = (declaring, method.name()) else {
            lowering
                .refusals
                .push(Refusal::GenericMethodNotHere { token: token.0 });
            continue;
        };
        let Some(label) = method_pair_label(assembly, &declaring, name, &arguments) else {
            lowering
                .refusals
                .push(Refusal::OpenMethodInstantiation { token: token.0 });
            continue;
        };
        pairs.push(MethodPair {
            def_row: definition.row(),
            arguments,
            label,
            tokens: alloc::vec![token],
        });
    }

    let mut deferred_targets: Vec<(Token, usize)> = Vec::new();
    for site in deferred {
        let Some(def_row) = site.def_row else {
            lowering.refusals.push(Refusal::NestedGenericMethodCall {
                owner: site.owner.clone(),
                token: site.token,
            });
            continue;
        };
        if let Some(position) = pairs
            .iter()
            .position(|pair| pair.def_row == def_row && pair.arguments == site.arguments)
        {
            deferred_targets.push((site.synthetic, position));
            continue;
        }
        let Some(method) = assembly.method(def_row) else {
            lowering.refusals.push(Refusal::NestedGenericMethodCall {
                owner: site.owner.clone(),
                token: site.token,
            });
            continue;
        };
        if method.flags() & super::METHOD_VIRTUAL != 0 {
            lowering.refusals.push(Refusal::VirtualGenericInDuplicatedBody {
                token: site.token,
                method: method.name().unwrap_or("").into(),
            });
            continue;
        }
        let rows = declaring_rows.get_or_insert_with(|| declaring_row_map(assembly));
        let label = rows
            .get(&def_row)
            .and_then(|&type_row| definition_key(assembly, Token::new(TYPE_DEF, type_row)))
            .zip(method.name())
            .and_then(|(declaring, name)| {
                method_pair_label(assembly, &declaring, name, &site.arguments)
            });
        let Some(label) = label else {
            lowering.refusals.push(Refusal::NestedGenericMethodCall {
                owner: site.owner.clone(),
                token: site.token,
            });
            continue;
        };
        deferred_targets.push((site.synthetic, pairs.len()));
        pairs.push(MethodPair {
            def_row,
            arguments: site.arguments.clone(),
            label,
            tokens: Vec::new(),
        });
    }

    expand_virtual_rows(
        assembly,
        sources,
        &virtual_rows,
        &mut declaring_rows,
        &mut pairs,
        &mut virtual_sites,
        lowering,
    );

    if pairs.is_empty() {
        return;
    }

    let mut emitted_ids: Vec<Option<MethodId>> = alloc::vec![None; pairs.len()];

    let rows = declaring_rows.unwrap_or_default();
    for (position, pair) in pairs.iter().enumerate() {
        let Some(method) = assembly.method(pair.def_row) else {
            if let Some(token) = pair.tokens.first() {
                lowering
                    .refusals
                    .push(Refusal::GenericMethodNotHere { token: token.0 });
            }
            continue;
        };
        let Some(raw) = method.body_and_bytes().map(|(_, raw)| raw) else {
            lowering.refusals.push(Refusal::BodyUnreadable {
                instantiation: pair.label.clone(),
                method: method.name().unwrap_or("").into(),
            });
            continue;
        };
        let Ok(plan) = plan_rewrite(assembly, raw, None) else {
            lowering.refusals.push(Refusal::BodyUnreadable {
                instantiation: pair.label.clone(),
                method: method.name().unwrap_or("").into(),
            });
            continue;
        };
        let mut refused = false;
        let cil = if plan.is_empty() {
            materialize(raw)
        } else {
            let mut bytes = raw.to_vec();
            for site in &plan {
                let synthetic = Token::new(SYNTHETIC_TABLE, *next_synthetic_row);
                *next_synthetic_row += 1;
                match bind_open_token(
                    module,
                    assembly,
                    asm,
                    type_offset,
                    sources,
                    PROGRAM_SOURCE,
                    type_index,
                    field_index,
                    instantiations,
                    emitted,
                    definition_methods,
                    &pair.label,
                    &[],
                    &pair.arguments,
                    None,
                    site,
                    synthetic,
                    &mut Vec::new(),
                ) {
                    Ok(()) => {
                        let at = site.operand_at;
                        bytes[at..at + 4].copy_from_slice(&synthetic.0.to_le_bytes());
                    }
                    Err(refusal) => {
                        lowering.refusals.push(refusal);
                        refused = true;
                    }
                }
            }
            owned_cil(bytes)
        };
        if refused {
            continue;
        }
        let id = module.add_method(asm, cil, arg_count(&method));
        emitted_ids[position] = Some(id);
        if let Some(type_id) = rows
            .get(&pair.def_row)
            .and_then(|&type_row| assembly.type_def(type_row))
            .and_then(|type_def| type_def.name())
            .and_then(|name| type_index.get(&type_name_key(name)).copied())
        {
            module.set_method_type(id, type_id);
        }
        #[cfg(feature = "debug-names")]
        module.set_method_debug(id, pair.label.clone(), Vec::new());
        for token in &pair.tokens {
            module.bind_token(asm, *token, id);
            module.clear_unlowered_generic(asm, *token);
        }
        for (synthetic, target) in &deferred_targets {
            if *target == position {
                module.bind_token(asm, *synthetic, id);
            }
        }
    }

    dispatch_virtual_sites(module, asm, &virtual_sites, &emitted_ids, lowering);
}

/// `MethodDef` rid -> the `TypeDef` row that declares it, over one assembly's whole method table.
///
/// One builder for both callers. It was written out twice, identically, which is the shape a new
/// case gets added to in one of them.
#[cfg(feature = "generics")]
fn declaring_row_map(assembly: &Assembly<'_>) -> BTreeMap<u32, u32> {
    let mut map = BTreeMap::new();
    let mut type_row = 0u32;
    for type_def in assembly.type_defs() {
        type_row += 1;
        for method in type_def.methods() {
            map.insert(method.rid(), type_row);
        }
    }
    map
}

/// One virtual generic call site, and the set of bodies a receiver may choose between at it.
///
/// # Why a site rather than a pair
///
/// Every other row this pass handles is one call site, one body, one bind. A virtual one is one call
/// site and N bodies -- the declaration's and every override's -- with which one runs decided by the
/// receiver. So the unit that has to succeed or fail TOGETHER is the site: binding a site whose
/// override failed to emit would dispatch a derived receiver to the base's body, which is the wrong
/// answer this axis exists to prevent, arriving through a partial success.
#[cfg(feature = "generics")]
struct VirtualSite {
    /// The `MethodSpec` token the call site names.
    ///
    /// **IT IS NEVER BOUND TO A BODY, AND THAT IS LOAD-BEARING RATHER THAN AN OMISSION.**
    /// `resolve_callvirt` tries the static target's vtable slot FIRST and falls back to the static
    /// target LAST, so a token bound to anything at all makes both of those reachable -- and the
    /// thing it would be bound to is the base's declaration, which is precisely the mis-bind. Left
    /// unbound, a dispatch that finds no key answers `None` and the interpreter traps
    /// `UnresolvedCall`. **The helpful fallback IS the failure mode.**
    token: Token,
    /// The method's name, for the refusal message.
    method: String,
    /// `(position in the pair list, the declaration's `MethodDef` row)`, seed first.
    bodies: Vec<(usize, u32)>,
    /// The key the ORDINARY LOAD already put in every type's dispatch map for this declaration.
    ///
    /// This pass does not walk the hierarchy to decide which type gets which body: it reads that
    /// answer back out of the map the load built, where inheritance is already flattened. A type
    /// that overrides answers its own declaration here; one that merely inherits answers its
    /// nearest ancestor's; an unrelated type that happens to declare the same member answers its
    /// own, which is in no lowered set and is therefore skipped. **One relation, computed once, by
    /// the code the run-time dispatch itself uses.**
    declaration_key: String,
    /// The key the call site and the lowered bodies share -- the declaration plus THIS site's type
    /// arguments, so `Tag<int>` and `Tag<string>` do not overwrite each other.
    key: String,
    /// `callvirt` is always on an instance: the parameters plus `this`.
    arg_count: u16,
}

/// The program's `MethodDef` row declaring `pair`, or `None` when the program does not declare it.
///
/// [`lamella_generics::MethodPair`] identifies a method by NAME, deliberately -- a metadata row is
/// an index into one assembly's tables and the closure crosses assemblies. This is the inverse, and
/// it is scoped to the PROGRAM on purpose: a body this pass emits is copied out of an assembly and
/// re-bound here, and doing that for an override declared next door is a different mechanism (the
/// owning assembly numbered its own vtable with no view of this module's call sites). A `None` is a
/// refusal at the call site, not a silent skip.
#[cfg(feature = "generics")]
fn locate_program_pair(
    assembly: &Assembly<'_>,
    pair: &lamella_generics::MethodPair,
) -> Option<u32> {
    for type_def in assembly.type_defs() {
        if definition_key(assembly, type_def.token()).as_deref() != Some(pair.declaring.as_ref()) {
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
            return Some(method.rid());
        }
    }
    None
}

/// Turns each virtual `MethodSpec` row into a set of bodies to emit and one [`VirtualSite`].
///
/// # The set is CLOSED, not read
///
/// `b.Tag<int>()` on a `Base b` emits ONE `MethodSpec` row, naming `Base::Tag`. The body that must
/// actually run when `b` holds a `Derived` is `Derived::Tag<int>`, and **nothing in the metadata
/// names it**. So the row is a seed and the set comes from `close_over_overrides`, which returns the
/// overrides and NOT the seed -- chained back in here rather than expected from it.
///
/// # Why `can_walk` is asked before the closure is believed
///
/// The closure looks a definition up by NAME, and **a name the closure cannot find yields ZERO
/// overrides and no error** -- which would emit the seed's body alone and read as a complete
/// answer. Asking whether the walk can see the declaring type turns that into a refusal at bake
/// time. What it guards is a declaring type in an assembly the walk was not given; a name SPELLED
/// differently on the two sides cannot arise, because every lookup here asks [`definition_key`]
/// and that is the closure's own speller.
#[cfg(feature = "generics")]
#[allow(clippy::too_many_arguments)]
fn expand_virtual_rows<'pe>(
    assembly: &Assembly<'pe>,
    sources: &[DefinitionSource<'pe>],
    virtual_rows: &[(Token, u32, Vec<SigType>)],
    declaring_rows: &mut Option<BTreeMap<u32, u32>>,
    pairs: &mut Vec<MethodPair>,
    virtual_sites: &mut Vec<VirtualSite>,
    lowering: &mut Lowering,
) {
    if virtual_rows.is_empty() {
        return;
    }
    let assemblies: Vec<Assembly<'pe>> = sources
        .iter()
        .map(|source| source.assembly.clone())
        .collect();
    let walk = lamella_generics::Program::new(&assemblies);
    let rows = declaring_rows.get_or_insert_with(|| declaring_row_map(assembly));

    for (token, def_row, arguments) in virtual_rows {
        let Some(method) = assembly.method(*def_row) else {
            lowering
                .refusals
                .push(Refusal::GenericMethodNotHere { token: token.0 });
            continue;
        };
        let name: String = method.name().unwrap_or("").into();
        let declaring = rows
            .get(def_row)
            .and_then(|&type_row| definition_key(assembly, Token::new(TYPE_DEF, type_row)));
        let (Some(declaring), Some(signature)) = (declaring, method.signature()) else {
            lowering.refusals.push(Refusal::VirtualGenericDeclarationUnreadable {
                token: token.0,
                method: name,
            });
            continue;
        };
        if !walk.can_walk(&declaring) {
            lowering.refusals.push(Refusal::VirtualGenericOverrideNotInThisProgram {
                token: token.0,
                method: name,
            });
            continue;
        }
        let seed = lamella_generics::MethodPair {
            declaring: declaring.as_str().into(),
            method: name.as_str().into(),
            parameters: signature.parameters.clone(),
            arity: signature.generic_param_count,
            arguments: arguments.clone(),
        };
        let mut planned: Vec<u32> = alloc::vec![*def_row];
        let mut refused = false;
        for found in walk.close_over_overrides(core::slice::from_ref(&seed)) {
            match locate_program_pair(assembly, &found) {
                Some(row) => planned.push(row),
                None => {
                    refused = true;
                    break;
                }
            }
        }
        if refused {
            lowering.refusals.push(Refusal::VirtualGenericOverrideNotInThisProgram {
                token: token.0,
                method: name,
            });
            continue;
        }

        let mut bodies: Vec<(usize, u32)> = Vec::new();
        for row in planned {
            let Some(declared) = assembly.method(row) else {
                refused = true;
                break;
            };
            if declared.body_and_bytes().is_none() {
                continue;
            }
            let owner = rows
                .get(&row)
                .and_then(|&type_row| definition_key(assembly, Token::new(TYPE_DEF, type_row)));
            let (Some(owner), Some(declared_name)) = (owner, declared.name()) else {
                refused = true;
                break;
            };
            let Some(label) = method_pair_label(assembly, &owner, declared_name, arguments) else {
                refused = true;
                break;
            };
            let position = match pairs
                .iter()
                .position(|pair| pair.def_row == row && pair.arguments == *arguments)
            {
                Some(position) => position,
                None => {
                    pairs.push(MethodPair {
                        def_row: row,
                        arguments: arguments.clone(),
                        label,
                        tokens: Vec::new(),
                    });
                    pairs.len() - 1
                }
            };
            bodies.push((position, row));
        }
        if refused || bodies.is_empty() {
            lowering.refusals.push(Refusal::VirtualGenericDeclarationUnreadable {
                token: token.0,
                method: name,
            });
            continue;
        }
        virtual_sites.push(VirtualSite {
            token: *token,
            declaration_key: super::sig_encode(
                assembly,
                &name,
                &signature.parameters,
                signature.generic_param_count,
                &[],
            ),
            key: super::sig_encode(
                assembly,
                &name,
                &signature.parameters,
                signature.generic_param_count,
                arguments,
            ),
            arg_count: arg_count(&method),
            method: name,
            bodies,
        });
    }
}

/// Puts each site's lowered bodies in the dispatch maps of the types that must reach them, and binds
/// the call site to the key they share.
///
/// # The three things it must NOT do, each of which succeeds and is wrong
///
/// - **Bind the `MethodSpec` token.** See [`VirtualSite::token`].
/// - **Give a lowered body a vtable slot.** `resolve_callvirt` checks `method_slot` before it
///   reaches the key, so a slot would restore token-order dispatch over the top of this.
/// - **Replace a type's map.** `set_sig_methods` writes the whole map; these entries go BESIDE what
///   the load put there, which is what [`Module::add_sig_method`] is for.
///
/// # What a gap looks like, at each layer
///
/// A body nothing dispatches to is refused here, at bake time, because it means the map this pass
/// read and the hierarchy the closure walked disagree. A type that should dispatch here and does not
/// appear in any lowered set is NOT detectable from this side -- it surfaces as `UnresolvedCall` at
/// the call site, which is loud and late. The one outcome that cannot occur is the silent one: with
/// the token unbound there is no body for a miss to fall back to.
///
/// **MEASURED, NOT ARGUED.** With `close_over_overrides` replaced by an empty list and nothing
/// else touched, `generic-method-virtual-program` traps `UnresolvedCall` on the `MethodSpec` --
/// **it does not answer 10.** That is the whole claim: the mis-bind this axis exists to prevent is
/// not merely absent, it is unreachable, because the route to it (the token's own binding) does not
/// exist for a virtual site.
#[cfg(feature = "generics")]
fn dispatch_virtual_sites(
    module: &mut Module,
    asm: u8,
    sites: &[VirtualSite],
    emitted_ids: &[Option<MethodId>],
    lowering: &mut Lowering,
) {
    for site in sites {
        let mut lowered: BTreeMap<MethodId, MethodId> = BTreeMap::new();
        let mut complete = true;
        for (position, def_row) in &site.bodies {
            let declaration = module.resolve(asm, Token::new(METHOD_DEF, *def_row));
            match (emitted_ids.get(*position).copied().flatten(), declaration) {
                (Some(body), Some(declaration)) => {
                    lowered.insert(declaration, body);
                }
                _ => complete = false,
            }
        }
        if !complete || lowered.is_empty() {
            lowering.refusals.push(Refusal::VirtualGenericBodyNotEmitted {
                token: site.token.0,
                method: site.method.clone(),
            });
            continue;
        }
        let declaration_key = module.intern_sig(&site.declaration_key);
        let mut targets: Vec<(TypeId, MethodId)> = Vec::new();
        let mut reached: BTreeSet<MethodId> = BTreeSet::new();
        let count = module.type_count() as TypeId;
        for type_id in 0..count {
            let Some(declared) = module.sig_dispatch(type_id, declaration_key) else {
                continue;
            };
            let Some(&body) = lowered.get(&declared) else {
                continue;
            };
            targets.push((type_id, body));
            reached.insert(body);
        }
        if reached.len() != lowered.len() {
            lowering.refusals.push(Refusal::VirtualGenericDispatchDiverged {
                token: site.token.0,
                method: site.method.clone(),
            });
            continue;
        }
        for (type_id, body) in targets {
            module.add_sig_method(type_id, &site.key, body);
        }
        module.bind_call_target(asm, site.token, site.key.clone(), site.arg_count);
        module.clear_unlowered_generic(asm, site.token);
    }
}

/// A definition's methods, in declaration order.
fn read_definition_methods<'pe>(assembly: &Assembly<'pe>, def_row: u32) -> Vec<DefMethod<'pe>> {
    let Some(type_def) = assembly.type_def(def_row) else {
        return Vec::new();
    };
    type_def
        .methods()
        .map(|method| {
            let signature = method.signature();
            DefMethod {
                name: method.name().unwrap_or("").into(),
                params: signature
                    .as_ref()
                    .map(|sig| sig.parameters.clone())
                    .unwrap_or_default(),
                arg_count: arg_count(&method),
                generic_arity: signature.as_ref().map_or(0, |sig| sig.generic_param_count),
                is_static: method.is_static(),
                is_virtual: method.flags() & super::METHOD_VIRTUAL != 0,
                newslot: method.flags() & super::METHOD_NEWSLOT != 0,
                raw: method.body_and_bytes().map(|(_, raw)| raw),
            }
        })
        .collect()
}

/// Where THIS instantiation's own storage went, for the bare `Field` tokens its copied body carries.
///
/// A `Field` token in a definition's own body names one of the definition's fields, and every copy
/// of that body has to reach the copy's storage instead: its own static cell (II.9.7) or its own
/// instance slot. Both lists are indexed by DECLARATION POSITION among fields of that kind, which is
/// what the two `..._position_by_row` helpers return.
#[derive(Clone, Copy)]
struct OwnerLayout<'a> {
    /// The definition's `TypeDef` row, in the assembly the body came from.
    def_row: u32,
    /// [`Emitted::own_static_slots`].
    statics: &'a [usize],
    /// [`Emitted::own_field_slots`].
    instance: &'a [u32],
    /// The instantiation's own type identity, which a bound instance field records as its declaring
    /// type.
    type_id: TypeId,
}

/// One token in a body that must mean something different per instantiation.
struct OpenSite {
    /// The byte offset, within the raw body, of the token operand's four bytes.
    operand_at: usize,
    /// The token as the definition wrote it.
    token: Token,
    /// The opcode that named it -- a type token binds differently for `newarr` than for `castclass`.
    opcode: Opcode,
}

/// Every token in `raw` whose resolution depends on the type arguments.
///
/// A token that mentions no type parameter is NOT a site: it already resolves, and leaving it alone
/// is both correct and free.
fn plan_rewrite<'pe>(assembly: &Assembly<'pe>, raw: &[u8], owner: Option<u32>) -> Result<Vec<OpenSite>, ()> {
    let layout = lamella_cil::body::read_body_layout(raw).map_err(|_| ())?;
    let code = raw
        .get(layout.code_offset..layout.code_offset + layout.code_len)
        .ok_or(())?;
    let (instructions, offsets) = lamella_cil::decode_with_offsets(code).map_err(|_| ())?;
    let mut sites = Vec::new();
    for (index, instruction) in instructions.iter().enumerate() {
        let lamella_cil::Operand::Token(token) = instruction.operand else {
            continue;
        };
        if !token_is_open(assembly, token, owner) {
            continue;
        }
        let end = offsets
            .get(index + 1)
            .map_or(layout.code_len, |next| *next as usize);
        let operand_at = layout.code_offset + end.checked_sub(4).ok_or(())?;
        sites.push(OpenSite {
            operand_at,
            token,
            opcode: instruction.opcode,
        });
    }
    Ok(sites)
}

/// Whether `token`'s meaning depends on the enclosing type's arguments.
///
/// `owner` is the `TypeDef` row of the generic DEFINITION whose body is being copied, when there is
/// one. It is what makes a bare `Field` token answerable -- see that arm.
fn token_is_open<'pe>(assembly: &Assembly<'pe>, token: Token, owner: Option<u32>) -> bool {
    match token.table() {
        FIELD => owner.is_some_and(|owner| {
            definition_static_position_by_row(assembly, owner, token.row()).is_some()
                || definition_field_position_by_row(assembly, owner, token.row()).is_some()
        }),
        TYPE_SPEC => assembly
            .type_spec_signature(token)
            .is_some_and(|sig| mentions_parameter(&sig)),
        MEMBER_REF => assembly
            .member_ref(token.row())
            .map(|member| member.parent())
            .filter(|parent| parent.table() == TYPE_SPEC)
            .and_then(|parent| assembly.type_spec_signature(parent))
            .is_some_and(|sig| mentions_parameter(&sig)),
        METHOD_SPEC => true,
        _ => false,
    }
}

/// Whether a signature names a type BY TOKEN anywhere inside it.
///
/// The question is not "is this a reference type" -- it is "does reading this signature require the
/// tables of the assembly that wrote it". `int`, `string` and `object` are element-type bytes in the
/// encoding and carry nothing; `Class`/`ValueType` carry a `TypeDefOrRef`, and a `GenericInst`
/// carries one for its definition whatever its arguments are.
///
/// MEASURED against the case this exists for: the self-check's two `TypeSpec` rows decode to
/// `15 12 0d 01 08` and `15 12 0d 01 0e` -- `List<int>` and `List<string>`, whose single arguments
/// are the primitive bytes `08` and `0e`. Neither reaches this function's `true` arm, which is why
/// the corlib case needs no rebase.
/// Whether a signature mentions a type parameter of either kind, anywhere inside it.
///
/// `pub(crate)` for the LOADER's benefit as well as this pass's, and sharing it is the point rather
/// than tidiness. The loader has to ask the same question of a `MemberRef`'s `TypeSpec` parent --
/// "is this an instantiation, or a definition naming itself?" -- and [`token_is_open`] already
/// answers it here for the COPY path. Two spellings of one rule is the shape where a new case
/// (another `SigType` that can carry a parameter) reaches one of them and not the other.
pub(crate) fn mentions_parameter(sig: &SigType) -> bool {
    match sig {
        SigType::Var(_) | SigType::MVar(_) => true,
        SigType::GenericInst {
            definition,
            arguments,
        } => mentions_parameter(definition) || arguments.iter().any(mentions_parameter),
        SigType::SzArray(inner) | SigType::Pointer(inner) | SigType::ByRef(inner) => {
            mentions_parameter(inner)
        }
        SigType::Array { element, .. } => mentions_parameter(element),
        _ => false,
    }
}

/// `sig` with `!n` replaced by `type_arguments[n]` and `!!n` by `method_arguments[n]`.
///
/// # The type axis passes an EMPTY `method_arguments`, and that is how its rule survives
///
/// A method type parameter's argument comes from a `MethodSpec` at the CALL SITE, so a type still
/// carrying `!!0` LOOKS closed and is not -- and a type that is not closed must not be layout-able.
/// **The type axis therefore still refuses every `!!n`, and does so as a CONSEQUENCE rather than as
/// a special case**: it passes an empty `method_arguments`, and `method_arguments.get(n)?` over an
/// empty slice is `None`. The caller that HAS the arguments is the only one that can resolve one.
///
/// Stating the rule as "who holds the arguments" rather than as "`MVar` is forbidden" is what lets
/// the method axis reuse [`plan_rewrite`] and [`bind_type_operand`] rather than grow a second walker
/// beside them.
///
/// `None` also for a parameter number with no argument, which is a malformed signature rather than
/// something to substitute a default into.
///
/// **`lamella_generics::substitute_sig` is the other half of this and is NOT generalized.** It is
/// the shared leaf both tiers consume and it refuses `!!n` unconditionally, which is right while the
/// AOT tier has no method axis and wrong the day it grows one. It is authoritative for the TYPE axis
/// in both tiers; this one is authoritative for the interpreter's method axis. The duplicate exists
/// because this module compiles without the optional `generics` dependency.
fn substitute_with(
    sig: &SigType,
    type_arguments: &[SigType],
    method_arguments: &[SigType],
) -> Option<SigType> {
    Some(match sig {
        SigType::Var(number) => type_arguments.get(*number as usize)?.clone(),
        SigType::MVar(number) => method_arguments.get(*number as usize)?.clone(),
        SigType::GenericInst {
            definition,
            arguments: inner,
        } => SigType::GenericInst {
            definition: Box::new(substitute_with(definition, type_arguments, method_arguments)?),
            arguments: inner
                .iter()
                .map(|argument| substitute_with(argument, type_arguments, method_arguments))
                .collect::<Option<Vec<_>>>()?,
        },
        SigType::Pointer(inner) => {
            SigType::Pointer(Box::new(substitute_with(inner, type_arguments, method_arguments)?))
        }
        SigType::ByRef(inner) => {
            SigType::ByRef(Box::new(substitute_with(inner, type_arguments, method_arguments)?))
        }
        SigType::SzArray(inner) => {
            SigType::SzArray(Box::new(substitute_with(inner, type_arguments, method_arguments)?))
        }
        SigType::Array { element, rank } => SigType::Array {
            element: Box::new(substitute_with(element, type_arguments, method_arguments)?),
            rank: *rank,
        },
        other => other.clone(),
    })
}

/// [`substitute_with`] for the TYPE axis: a type's arguments and no method arguments at all.
fn substitute(sig: &SigType, arguments: &[SigType]) -> Option<SigType> {
    substitute_with(sig, arguments, &[])
}

/// A generic CALL found inside a duplicated body, whose arguments the ENCLOSING instantiation has
/// just closed -- recorded in phase 3 and bound in phase 6.
///
/// # Why it cannot be bound where it is found
///
/// `class Box<T>` calling `Helper.Zero<T>()` emits ONE `MethodSpec` whose argument is `!0`. It is
/// not a pair until an instantiation supplies the `T`, and then it is a DIFFERENT pair for each:
/// `Box<string>` needs `Helper.Zero<string>` and `Box<int>` needs `Helper.Zero<int>`. So the pair is
/// discovered while copying a body (phase 3) and can only be EMITTED once the method axis runs
/// (phase 6), which is the same shape as phase 1 creating every type identity before any body
/// exists -- a body may name something the pass has not made yet.
///
/// The token in the copied body is already rewritten to `synthetic`; all that is outstanding is what
/// `synthetic` resolves to. **A site recorded here and never bound is a body naming a token that
/// resolves to nothing**, which the bake refuses -- so the failure mode of a missed one is loud.
///
/// Without the `generics` capability the instantiation set is empty, so no body is ever copied, so
/// nothing is ever recorded here and `lower_method_pairs` is a no-op -- every field is written by
/// code that cannot run and read by code that is compiled out.
#[cfg_attr(not(feature = "generics"), allow(dead_code))]
struct DeferredMethodSite {
    /// The synthetic token the copied body now names.
    synthetic: Token,
    /// The `MethodSpec` token as the definition wrote it, for refusals.
    token: u32,
    /// The pair whose body named it.
    owner: String,
    /// The generic method's `MethodDef` row, if it is one of this assembly's.
    def_row: Option<u32>,
    /// The call-site arguments with the enclosing instantiation's substituted in -- CLOSED, or the
    /// site would not have been recorded.
    arguments: Vec<SigType>,
}

/// The dispatch key a member of an instantiation answers to: its name and its parameter types with
/// the instantiation's arguments substituted in.
fn substituted_sig_key<'pe>(
    assembly: &Assembly<'pe>,
    name: &str,
    params: &[SigType],
    arguments: &[SigType],
    generic_arity: u32,
) -> Option<String> {
    let substituted: Vec<SigType> = params
        .iter()
        .map(|param| substitute(param, arguments))
        .collect::<Option<_>>()?;
    Some(super::sig_encode(assembly, name, &substituted, generic_arity, &[]))
}

/// The zero value one substituted field signature takes.
fn default_field_value_substituted<'pe>(
    module: &Module,
    assembly: &Assembly<'pe>,
    asm: u8,
    type_offset: Option<usize>,
    type_index: &TypeNameIndex,
    sig: &SigType,
    enum_zeros: &BTreeMap<String, Value>,
) -> Value {
    if let SigType::ValueType(token) = sig {
        if let Some(name) = assembly.type_token_name(*token) {
            if let Some(zero) = enum_zeros.get(&type_name_key(name)) {
                return zero.clone();
            }
        }
    }
    struct_zero_of_sig(module, assembly, asm, type_offset, type_index, sig)
        .unwrap_or_else(|| default_field_value(Some(sig.clone())))
}

/// A zero INSTANCE of a substituted signature naming a STRUCT, or `None` when the signature names
/// something else or the struct cannot be resolved here.
///
/// [`default_field_value`] answers null for every value type that is not a primitive, which is
/// correct for a reference and wrong for a struct: ECMA-335 III.4.21 zero-initializes an instance,
/// so a `decimal` or `DateTime` field of an instantiation starts as a zeroed struct. Left null it
/// traps the moment an intrinsic reads it -- `Holder<int>.Fee == 0m` on a fresh instance.
///
/// Resolvable here because monomorphization runs after the ordinary walk, so an ordinary struct's
/// layout is already final. An instantiation whose field is ANOTHER instantiation still being
/// built in this same pass is not, and keeps the null it had.
fn struct_zero_of_sig<'pe>(
    module: &Module,
    assembly: &Assembly<'pe>,
    asm: u8,
    type_offset: Option<usize>,
    type_index: &TypeNameIndex,
    sig: &SigType,
) -> Option<Value> {
    let SigType::ValueType(token) = sig else {
        return None;
    };
    let type_id = base_type_of(module, assembly, asm, type_offset, type_index, *token)?;
    if !module.type_is_value_type(type_id) {
        return None;
    }
    Some(Value::Struct(
        module.type_field_defaults(type_id)?.into_boxed_slice(),
    ))
}

/// The `TypeId` a definition's `extends` token names, if this load can see it.
///
/// `type_offset` is the module type index the definition's ASSEMBLY starts at, and both arms are
/// about that assembly: a `TypeDef` token is a row in it, a `TypeRef` names a type somewhere else
/// by name. Passing the program's offset for a corlib-declared definition is how `List<T>`'s base
/// would come back as whichever program type sits at the corlib's `System.Object` row.
///
/// `None` means the assembly's types were not loaded contiguously -- a tier that materializes them
/// on demand has no such offset, and the module's own per-assembly token map answers instead.
fn base_type_of<'pe>(
    module: &Module,
    assembly: &Assembly<'pe>,
    asm: u8,
    type_offset: Option<usize>,
    type_index: &TypeNameIndex,
    extends: Token,
) -> Option<TypeId> {
    match extends.table() {
        TYPE_DEF => match type_offset {
            Some(offset) => {
                let index = (extends.row() as usize).checked_sub(1)?;
                Some((offset + index) as TypeId)
            }
            None => module.type_id_of(asm, extends),
        },
        TYPE_REF => assembly
            .type_token_name(extends)
            .and_then(|name| type_index.get(&type_name_key(name)).copied()),
        _ => None,
    }
}

/// The full name, with its arity backtick, of the one generic definition ECMA-335 special-cases in
/// the instruction set.
const NULLABLE_DEFINITION: &str = "System.Nullable`1";

/// Records `Nullable<T>`'s underlying type on the instantiation it just created, so the four
/// instruction-set arms that special-case a nullable can find `T`.
///
/// # Why the whole special case reduces to one recorded handle
///
/// ECMA-335 4th ed gives `System.Nullable<T>` its own clause in `box` (III.4.1), `castclass`
/// (III.4.3), `isinst` (III.4.6), `unbox` (III.4.32) and `unbox.any` (III.4.33), and every one of
/// them turns on the same two questions: is the operand type a nullable, and what is `T`. Recorded
/// here, `Some(handle)` answers both -- and it is recorded where the type IDENTITY is created
/// rather than where a token names it, because a token that names an instantiation is minted in
/// more than one place while the identity is minted exactly once.
///
/// A definition whose name is not `Nullable<T>`, or a type argument no loaded assembly declares,
/// records nothing: the arms then treat the type as the ordinary value type it looks like.
///
/// # Why the PROGRAM's own `System.Nullable<T>` is excluded
///
/// A program may declare a type of that exact name -- csc allows it with CS0436 and binds the
/// program's -- and .NET does NOT give the shadow the runtime's special case, because the CLI
/// identifies the type from the core library rather than by name. Measured, on a program declaring
/// its own `System.Nullable<Point>`: .NET boxes an instance into a boxed
/// `` System.Nullable`1[Point] `` and the following `(Point)` cast throws
/// *"Unable to cast object of type 'System.Nullable`1[Point]' to type 'Point'"* -- so III.4.1's
/// nullable clause did not apply to it. Keying on the name alone would make this tier answer 7654321
/// where .NET throws.
fn bind_nullable_underlying<'pe>(
    module: &mut Module,
    program: &Assembly<'pe>,
    type_index: &TypeNameIndex,
    want: &Instantiation,
    type_id: TypeId,
    source: usize,
) {
    if source == PROGRAM_SOURCE
        || want.definition != NULLABLE_DEFINITION
        || want.arguments.len() != 1
    {
        return;
    }
    let key = match &want.arguments[0] {
        argument @ (SigType::Boolean
        | SigType::Char
        | SigType::I1
        | SigType::U1
        | SigType::I2
        | SigType::U2
        | SigType::I4
        | SigType::U4
        | SigType::I8
        | SigType::U8
        | SigType::R4
        | SigType::R8
        | SigType::IntPtr
        | SigType::UIntPtr) => type_key("System", primitive_display_name(argument)),
        SigType::Class(token) | SigType::ValueType(token) => {
            match program.type_token_name(*token) {
                Some(name) => type_name_key(name),
                None => return,
            }
        }
        _ => return,
    };
    let Some(&underlying_id) = type_index.get(&key) else {
        return;
    };
    if let Some(handle) = module.type_handle_of(underlying_id) {
        module.bind_nullable_underlying(type_id, handle);
    }
}

/// Marks a bound `.ctor` token as constructing a VALUE TYPE, so `newobj` builds the struct in
/// place instead of allocating a heap instance (III.4.2).
///
/// # Why the ordinary marking pass cannot reach these tokens
///
/// `mark_value_type_ctors` runs while each assembly LOADS, and answers for a `MemberRef` by
/// resolving it and asking what its method declares. An instantiation's `.ctor` is bound by THIS
/// pass, which runs after every assembly has loaded -- so at marking time the token resolves to
/// nothing and is silently left on the reference path. The mark therefore belongs beside the
/// binding, which is the only point where both halves are known.
///
/// A non-`.ctor` member is left alone: the mark is read only by `newobj`, and a `call` to an
/// instance method of a struct addresses a receiver it did not create.
fn mark_value_type_newobj(
    module: &mut Module,
    asm: u8,
    token: Token,
    member_name: &str,
    is_value_type: bool,
) {
    if is_value_type && member_name == ".ctor" {
        module.mark_value_type_ctor(asm, token);
    }
}

/// Finds the instantiation of `definition` with exactly `arguments`.
///
/// The ARGUMENTS are matched STRUCTURALLY, on their decoded signatures, rather than through a name
/// this pass would have had to spell. A comparison is the one form of identity that cannot drift
/// from someone else's spelling rule.
///
/// **THE DEFINITION HALF IS STILL A NAME, AND IT IS NOT THIS PASS'S TO SPELL.** `definition` must
/// come from [`definition_key`], which is the speller that wrote every
/// [`Instantiation::definition`] this searches.
fn find_instantiation(
    instantiations: &[Instantiation],
    emitted: &[(usize, Emitted)],
    definition: &str,
    arguments: &[SigType],
) -> Option<usize> {
    emitted.iter().position(|(index, _)| {
        let want = &instantiations[*index];
        want.definition == definition && want.arguments == arguments
    })
}

/// [`find_instantiation`] by CANONICAL NAME first, falling back to the token-equality match.
///
/// # Why a name, and why the token match still stands behind it
///
/// The token match compares `SigType`s, and **a token means nothing outside the assembly that wrote
/// it** -- so it can only ever find an instantiation whose arguments were spelled in the SAME world
/// as the call site's -- the arguments are the program's and the definition is somebody else's, and
/// there is no one world to compare in.
///
/// A canonical NAME has no world. `lamella_generics::spell_sig_across` decodes each side against
/// its own assembly and composes after, which is the same thing the AOT tier's resolver does at its
/// own two-world site -- so the two tiers agree on a spelling by construction rather than by
/// coincidence.
///
/// **The token path is kept as the fallback rather than replaced**, because a name cannot always be
/// spelled: a site carrying a METHOD type parameter substitutes from an argument list the speller
/// does not take, and answers `None`. Those sites match exactly as they did before.
#[cfg(feature = "generics")]
fn find_instantiation_named(
    instantiations: &[Instantiation],
    emitted: &[(usize, Emitted)],
    definition: &str,
    arguments: &[SigType],
    spelled: Option<&str>,
) -> Option<usize> {
    if let Some(name) = spelled {
        if let Some(found) = emitted
            .iter()
            .position(|(index, _)| instantiations[*index].name == name)
        {
            return Some(found);
        }
    }
    find_instantiation(instantiations, emitted, definition, arguments)
}

/// The canonical name of `open` instantiated with `arguments`, with the two sides read against the
/// assemblies they were actually written in -- the definition against `definition_assembly`, every
/// argument against `argument_assembly`. `None` without the `generics` capability, or when either
/// side does not decode (a method type parameter, most often).
#[cfg(feature = "generics")]
fn spell_instantiation<'pe>(
    definition_assembly: &Assembly<'pe>,
    argument_assembly: &Assembly<'pe>,
    open: &SigType,
    arguments: &[SigType],
) -> Option<String> {
    lamella_generics::spell_sig_across(definition_assembly, argument_assembly, open, arguments)
}

/// Binds one rewritten token to what it means for THIS instantiation or pair.
///
/// **BOTH AXES CALL THIS, AND THE ONLY DIFFERENCE IS WHICH ARGUMENT LIST IS EMPTY.** The type axis
/// passes its instantiation's arguments and no method arguments; the method axis passes the call
/// site's arguments as the method half. Everything else -- the `TypeSpec` arm, the `MemberRef` arm,
/// the operand binding -- is one implementation, which is what keeps a `newarr` of a method type
/// parameter getting the same element-zero treatment a `newarr` of a type parameter already got.
#[allow(clippy::too_many_arguments)]
fn bind_open_token<'pe>(
    module: &mut Module,
    assembly: &Assembly<'pe>,
    asm: u8,
    type_offset: Option<usize>,
    sources: &[DefinitionSource<'pe>],
    owner_source: usize,
    type_index: &TypeNameIndex,
    field_index: &FieldNameIndex,
    instantiations: &[Instantiation],
    emitted: &[(usize, Emitted)],
    definition_methods: &BTreeMap<(usize, u32), Vec<DefMethod<'pe>>>,
    owner: &str,
    type_arguments: &[SigType],
    method_arguments: &[SigType],
    owner_statics: Option<OwnerLayout<'_>>,
    site: &OpenSite,
    synthetic: Token,
    deferred: &mut Vec<DeferredMethodSite>,
) -> Result<(), Refusal> {
    let unbound = || Refusal::UnboundAfterSubstitution {
        instantiation: owner.into(),
        token: site.token.0,
    };
    match site.token.table() {
        FIELD => {
            let layout = owner_statics.ok_or_else(unbound)?;
            if let Some(position) =
                definition_static_position_by_row(assembly, layout.def_row, site.token.row())
            {
                let slot = layout.statics.get(position).copied().ok_or_else(unbound)?;
                module.bind_static_field_ref(asm, synthetic, slot);
                return Ok(());
            }
            let slot = definition_field_position_by_row(assembly, layout.def_row, site.token.row())
                .and_then(|position| layout.instance.get(position).copied())
                .ok_or_else(unbound)?;
            module.bind_field(asm, synthetic, slot);
            module.bind_field_type(asm, synthetic, layout.type_id);
            Ok(())
        }
        METHOD_SPEC if !method_arguments.is_empty() => Err(Refusal::NestedGenericMethodCall {
            owner: owner.into(),
            token: site.token.0,
        }),
        METHOD_SPEC if owner_source != PROGRAM_SOURCE => {
            Err(Refusal::MethodSpecFromAnotherAssembly {
                owner: owner.into(),
                token: site.token.0,
            })
        }
        METHOD_SPEC => {
            let arguments = assembly
                .method_spec_instantiation(site.token)
                .ok_or_else(unbound)?
                .iter()
                .map(|argument| substitute_with(argument, type_arguments, method_arguments))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| Refusal::MethodTypeParameter {
                    instantiation: owner.into(),
                    token: site.token.0,
                })?;
            if arguments.iter().any(mentions_parameter) {
                return Err(Refusal::OpenMethodInstantiation { token: site.token.0 });
            }
            let definition = assembly.method_spec_method(site.token).ok_or_else(unbound)?;
            deferred.push(DeferredMethodSite {
                synthetic,
                token: site.token.0,
                owner: owner.into(),
                def_row: (definition.table() == METHOD_DEF).then(|| definition.row()),
                arguments,
            });
            Ok(())
        }
        TYPE_SPEC => {
            let sig = assembly
                .type_spec_signature(site.token)
                .ok_or_else(unbound)?;
            let substituted = substitute_with(&sig, type_arguments, method_arguments)
                .ok_or_else(|| Refusal::MethodTypeParameter {
                    instantiation: owner.into(),
                    token: site.token.0,
                })?;
            bind_type_operand(
                module,
                assembly,
                asm,
                type_offset,
                type_index,
                instantiations,
                emitted,
                owner,
                site,
                synthetic,
                &substituted,
            )
        }
        MEMBER_REF => {
            let member = assembly.member_ref(site.token.row()).ok_or_else(unbound)?;
            let parent_sig = assembly
                .type_spec_signature(member.parent())
                .ok_or_else(unbound)?;
            #[cfg(feature = "generics")]
            let spelled = spell_instantiation(
                assembly,
                &sources[PROGRAM_SOURCE].assembly,
                &parent_sig,
                type_arguments,
            );
            let SigType::GenericInst {
                definition,
                arguments,
            } = parent_sig
            else {
                return Err(unbound());
            };
            let definition_token = match definition.as_ref() {
                SigType::Class(token) | SigType::ValueType(token) => *token,
                _ => return Err(unbound()),
            };
            let definition_name = definition_key(assembly, definition_token).ok_or_else(unbound)?;
            let substituted: Vec<SigType> = arguments
                .iter()
                .map(|argument| substitute_with(argument, type_arguments, method_arguments))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| Refusal::MethodTypeParameter {
                    instantiation: owner.into(),
                    token: site.token.0,
                })?;
            #[cfg(feature = "generics")]
            let target =
                find_instantiation_named(instantiations, emitted, &definition_name, &substituted, spelled.as_deref())
                    .ok_or_else(|| Refusal::InstantiationNotInSet {
                        instantiation: owner.into(),
                        wanted: definition_name.clone(),
                    })?;
            #[cfg(not(feature = "generics"))]
            let target = find_instantiation(instantiations, emitted, &definition_name, &substituted)
                .ok_or_else(|| Refusal::InstantiationNotInSet {
                    instantiation: owner.into(),
                    wanted: definition_name.clone(),
                })?;
            let (target_index, target_entry) = &emitted[target];
            let target_want = &instantiations[*target_index];
            let target_assembly = &sources[target_entry.source].assembly;
            let name = member.name().unwrap_or("");
            if member.is_field() {
                if let Some(position) =
                    definition_static_position(target_assembly, target_entry.def_row, name)
                {
                    let slot = target_entry
                        .own_static_slots
                        .get(position)
                        .copied()
                        .ok_or_else(unbound)?;
                    module.bind_static_field_ref(asm, synthetic, slot);
                    return Ok(());
                }
                let slot = definition_field_position(target_assembly, target_entry.def_row, name)
                    .and_then(|position| target_entry.own_field_slots.get(position).copied())
                    .ok_or_else(unbound)?;
                let _ = field_index;
                module.bind_field(asm, synthetic, slot);
                module.bind_field_type(asm, synthetic, target_entry.type_id);
                return Ok(());
            }
            let params = member
                .method_signature()
                .map(|sig| sig.parameters)
                .unwrap_or_default();
            let methods = definition_methods
                .get(&(target_entry.source, target_entry.def_row))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let position = methods
                .iter()
                .position(|method| method.name == name && method.params == params)
                .ok_or_else(unbound)?;
            let id = target_entry
                .methods
                .get(position)
                .copied()
                .flatten()
                .ok_or_else(unbound)?;
            module.bind_token(asm, synthetic, id);
            mark_value_type_newobj(module, asm, synthetic, name, target_entry.is_value_type);
            if let Some(key) = substituted_sig_key(
                target_assembly,
                name,
                &methods[position].params,
                &target_want.arguments,
                methods[position].generic_arity,
            ) {
                let count = u16::try_from(params.len() + 1).unwrap_or(u16::MAX);
                module.bind_call_target(asm, synthetic, key, count);
            }
            Ok(())
        }
        _ => Err(unbound()),
    }
}

/// Binds a rewritten TYPE token -- the operand of `newobj`'s type, `castclass`, `isinst`, `box`,
/// `newarr`, `ldtoken`, `initobj`, `constrained.`.
#[allow(clippy::too_many_arguments)]
fn bind_type_operand<'pe>(
    module: &mut Module,
    assembly: &Assembly<'pe>,
    asm: u8,
    type_offset: Option<usize>,
    type_index: &TypeNameIndex,
    instantiations: &[Instantiation],
    emitted: &[(usize, Emitted)],
    owner: &str,
    site: &OpenSite,
    synthetic: Token,
    substituted: &SigType,
) -> Result<(), Refusal> {
    let unbound = || Refusal::UnboundAfterSubstitution {
        instantiation: owner.into(),
        token: site.token.0,
    };

    module.bind_cast_elem(asm, synthetic, cast_elem_of_sig(asm, substituted));
    if matches!(site.opcode, Opcode::Newarr) {
        let zero = struct_zero_of_sig(module, assembly, asm, type_offset, type_index, substituted)
            .unwrap_or_else(|| default_field_value(Some(substituted.clone())));
        module.bind_array_default(asm, synthetic, zero);
    }
    match substituted {
        SigType::GenericInst {
            definition,
            arguments,
        } => {
            let definition_token = match definition.as_ref() {
                SigType::Class(token) | SigType::ValueType(token) => *token,
                _ => return Err(unbound()),
            };
            let definition_name = definition_key(assembly, definition_token).ok_or_else(unbound)?;
            let target = find_instantiation(instantiations, emitted, &definition_name, arguments)
                .ok_or_else(|| Refusal::InstantiationNotInSet {
                    instantiation: owner.into(),
                    wanted: definition_name.clone(),
                })?;
            let (target_index, target_entry) = &emitted[target];
            module.bind_type_token(asm, synthetic, target_entry.type_id);
            module.bind_type_name(asm, synthetic, instantiations[*target_index].name.clone());
            Ok(())
        }
        SigType::Class(token) | SigType::ValueType(token) => {
            let name = assembly.type_token_name(*token).ok_or_else(unbound)?;
            if let Some(&type_id) = type_index.get(&type_name_key(name)) {
                module.bind_type_token(asm, synthetic, type_id);
            }
            module.bind_type_name(asm, synthetic, name.name.into());
            Ok(())
        }
        other => {
            let display = primitive_display_name(other);
            if !display.is_empty() {
                if let Some(&type_id) = type_index.get(&type_key("System", display)) {
                    module.bind_type_token(asm, synthetic, type_id);
                }
            }
            module.bind_type_name(asm, synthetic, display.into());
            Ok(())
        }
    }
}

/// The declaration position of one of a definition's own INSTANCE fields.
fn definition_field_position<'pe>(assembly: &Assembly<'pe>, def_row: u32, name: &str) -> Option<usize> {
    let type_def = assembly.type_def(def_row)?;
    type_def
        .fields()
        .filter(|field| !field.is_static())
        .position(|field| field.name() == Some(name))
}

/// The same position as [`definition_static_position`], found by the field's own `Field` ROW rather
/// than by name -- for a bare `Field` token in the definition's own body, which carries a row and no
/// name to match on.
///
/// Walks `def_row`'s own field list and counts, so a token belonging to some OTHER type answers
/// `None` and is left alone. That is the whole guard: a body may name any field in the assembly, and
/// only its own declaring type's statics move per instantiation.
fn definition_static_position_by_row<'pe>(
    assembly: &Assembly<'pe>,
    def_row: u32,
    field_row: u32,
) -> Option<usize> {
    assembly
        .type_def(def_row)?
        .fields()
        .filter(|field| field.is_static() && !field.is_literal())
        .position(|field| field.token().row() == field_row)
}

/// The declaration position of one of a definition's own INSTANCE fields, found by the field's
/// metadata ROW rather than by its name -- indexing [`Emitted::own_field_slots`].
///
/// The row form is what a bare `Field` token in the definition's own body carries. The name form
/// beside it is what a `MemberRef` from OUTSIDE carries, and they cannot be one function: a token
/// row is meaningless across assemblies and a name is meaningless without one.
fn definition_field_position_by_row<'pe>(
    assembly: &Assembly<'pe>,
    def_row: u32,
    field_row: u32,
) -> Option<usize> {
    assembly
        .type_def(def_row)?
        .fields()
        .filter(|field| !field.is_static())
        .position(|field| field.token().row() == field_row)
}

/// The declaration position of one of a definition's own non-literal STATIC fields, indexing
/// [`Emitted::own_static_slots`].
///
/// The two filters are the mirror of each other and both are load-bearing: this one skips instance
/// fields AND literals, exactly as the reservation loop in phase 2 does, so position `n` here is the
/// slot reserved `n`th there. A literal has no storage (II.16.1 -- its value is in the metadata and
/// every read is folded), so counting one would shift every slot after it by one.
fn definition_static_position<'pe>(
    assembly: &Assembly<'pe>,
    def_row: u32,
    name: &str,
) -> Option<usize> {
    let type_def = assembly.type_def(def_row)?;
    type_def
        .fields()
        .filter(|field| field.is_static() && !field.is_literal())
        .position(|field| field.name() == Some(name))
}

/// The simple name a primitive `SigType` displays under, for `typeof(T).Name`.
/// `pub(crate)` because the LAZY corlib tier needs the same mapping: a primitive type argument may be
/// BOXED by the definition's body, and a box whose corlib type was never materialized has no id for a
/// `callvirt` to resolve its receiver through.
pub(crate) fn primitive_display_name(sig: &SigType) -> &'static str {
    match sig {
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
        SigType::Object => "Object",
        _ => "",
    }
}

/// Binds every CLOSED `TypeSpec` row naming one of these instantiations to its type identity, so a
/// `castclass` / `isinst` / `ldtoken` / `box` of `Pair<int,string>` reaches the type this pass made.
fn bind_closed_type_specs<'pe>(
    module: &mut Module,
    assembly: &Assembly<'pe>,
    asm: u8,
    instantiations: &[Instantiation],
    emitted: &[(usize, Emitted)],
) {
    for row in 1..=u32::from(u16::MAX) {
        let token = Token::new(TYPE_SPEC, row);
        let Some(sig) = assembly.type_spec_signature(token) else {
            break;
        };
        let SigType::GenericInst {
            definition,
            arguments,
        } = &sig
        else {
            continue;
        };
        let definition_token = match definition.as_ref() {
            SigType::Class(token) | SigType::ValueType(token) => *token,
            _ => continue,
        };
        let Some(name) = definition_key(assembly, definition_token) else {
            continue;
        };
        let Some(target) = find_instantiation(instantiations, emitted, &name, arguments) else {
            continue;
        };
        let (index, entry) = &emitted[target];
        module.bind_type_token(asm, token, entry.type_id);
        module.bind_type_name(asm, token, instantiations[*index].name.clone());
        module.bind_cast_elem(
            asm,
            token,
            lamella_cil_runtime::CastElem::Named(lamella_cil_runtime::module::asm_key(asm, token.0)),
        );
        #[cfg(feature = "exceptions")]
        {
            let tag = lamella_cil_runtime::exception::exception_tag(&instantiations[*index].name);
            module.bind_catch_type_tag(asm, token, tag);
            module.clear_unlowered_generic(asm, token);
        }
    }
}

/// The first pair of instantiations in this set that mint the SAME exception tag, if any.
///
/// Checked over the set that is actually being LOWERED, which is the set whose names can reach a
/// `catch`. Two of them under one tag means a `catch (A<int>)` accepts a `B<string>`, silently --
/// there is no cast to fail and no trap to raise, only a handler that runs.
///
/// **Quadratic on purpose.** The set is the closed instantiations of one program, and the honest
/// comparison is name-against-name; a hash-set of tags would find a collision but could not name
/// the SECOND participant, and a refusal that cannot say what it collided with is one nobody can
/// act on. Sorted-first so the pair reported is stable run to run rather than dependent on
/// collection order.
///
/// **WHAT THE RED-PROOF COVERS, AND WHAT IT CANNOT.** Forcing the comparison true made the pair
/// fixture refuse with BOTH participants named, nothing lowered, and the bake stopping -- so
/// detection, the refusal, the early return and the marks standing are all exercised. **It says
/// nothing about whether FNV-32 collides on any real pair.** Nothing short of an actual collision
/// can, which is precisely why this is a check rather than an argument that 31 bits is enough.
#[cfg(feature = "exceptions")]
fn exception_tag_collision(
    instantiations: &[Instantiation],
    emitted: &[(usize, Emitted)],
) -> Option<Refusal> {
    let mut named: Vec<&str> = emitted
        .iter()
        .map(|(index, _)| instantiations[*index].name.as_str())
        .collect();
    named.sort_unstable();
    named.dedup();
    for (position, first) in named.iter().enumerate() {
        let tag = lamella_cil_runtime::exception::exception_tag(first);
        for second in &named[position + 1..] {
            if lamella_cil_runtime::exception::exception_tag(second) == tag {
                return Some(Refusal::ExceptionTagCollision {
                    first: (*first).into(),
                    second: (*second).into(),
                    tag,
                });
            }
        }
    }
    None
}

/// Binds every `MemberRef` reached through a CLOSED instantiation to the member of the type this
/// pass made, and withdraws that token's `UnloweredGeneric` mark -- but ONLY once it is bound.
fn bind_closed_member_refs<'pe>(
    module: &mut Module,
    assembly: &Assembly<'pe>,
    asm: u8,
    sources: &[DefinitionSource<'pe>],
    instantiations: &[Instantiation],
    emitted: &[(usize, Emitted)],
    definition_methods: &BTreeMap<(usize, u32), Vec<DefMethod<'pe>>>,
    lowering: &mut Lowering,
) {
    for row in 1..=u32::from(u16::MAX) {
        let token = Token::new(MEMBER_REF, row);
        let Some(member) = assembly.member_ref(row) else {
            break;
        };
        let parent = member.parent();
        if parent.table() != TYPE_SPEC {
            continue;
        }
        let Some(SigType::GenericInst {
            definition,
            arguments,
        }) = assembly.type_spec_signature(parent)
        else {
            continue;
        };
        let definition_token = match definition.as_ref() {
            SigType::Class(token) | SigType::ValueType(token) => *token,
            _ => continue,
        };
        let Some(definition_name) = definition_key(assembly, definition_token) else {
            continue;
        };
        let Some(target) = find_instantiation(instantiations, emitted, &definition_name, &arguments)
        else {
            continue;
        };
        let (index, entry) = &emitted[target];
        let want = &instantiations[*index];
        let definition_assembly = &sources[entry.source].assembly;
        let name = member.name().unwrap_or("");
        if member.is_field() {
            if let Some(position) =
                definition_static_position(definition_assembly, entry.def_row, name)
            {
                let Some(slot) = entry.own_static_slots.get(position).copied() else {
                    lowering.refusals.push(Refusal::UnboundAfterSubstitution {
                        instantiation: want.name.clone(),
                        token: token.0,
                    });
                    continue;
                };
                module.bind_static_field_ref(asm, token, slot);
                module.clear_unlowered_generic(asm, token);
                continue;
            }
            let Some(slot) = definition_field_position(definition_assembly, entry.def_row, name)
                .and_then(|position| entry.own_field_slots.get(position).copied())
            else {
                lowering.refusals.push(Refusal::UnboundAfterSubstitution {
                    instantiation: want.name.clone(),
                    token: token.0,
                });
                continue;
            };
            module.bind_field(asm, token, slot);
            module.bind_field_type(asm, token, entry.type_id);
            module.clear_unlowered_generic(asm, token);
            continue;
        }
        let params = member
            .method_signature()
            .map(|sig| sig.parameters)
            .unwrap_or_default();
        let methods = definition_methods
            .get(&(entry.source, entry.def_row))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let Some(position) = methods
            .iter()
            .position(|method| method.name == name && method.params == params)
        else {
            lowering.refusals.push(Refusal::UnboundAfterSubstitution {
                instantiation: want.name.clone(),
                token: token.0,
            });
            continue;
        };
        let bodyless = methods[position].raw.is_none();
        let id = match entry.methods.get(position).copied().flatten() {
            Some(id) => Some(id),
            None if bodyless => None,
            None => {
                lowering.refusals.push(Refusal::UnboundAfterSubstitution {
                    instantiation: want.name.clone(),
                    token: token.0,
                });
                continue;
            }
        };
        if let Some(id) = id {
            module.bind_token(asm, token, id);
            mark_value_type_newobj(module, asm, token, name, entry.is_value_type);
        }
        let key = substituted_sig_key(
            definition_assembly,
            name,
            &methods[position].params,
            &want.arguments,
            methods[position].generic_arity,
        );
        if let Some(key) = key.clone() {
            let count = u16::try_from(params.len() + 1).unwrap_or(u16::MAX);
            module.bind_call_target(asm, token, key, count);
        }
        if id.is_none() && key.is_none() {
            lowering.refusals.push(Refusal::UnboundAfterSubstitution {
                instantiation: want.name.clone(),
                token: token.0,
            });
            continue;
        }
        module.clear_unlowered_generic(asm, token);
    }
}

/// A body whose bytes were REWRITTEN: it must be owned, because it no longer matches anything in
/// the PE and there is nothing left to borrow.
///
/// Under XIP this is [`RawCil::Ram`] rather than a leaked `&'static`, which matters: residence is
/// a property of the VALUE here, so a rewritten body is freed with the module that holds it instead
/// of outliving it. Only bodies with NO open token stay flash-borrowed -- and this pass leaves those
/// to the loader's own materializer, so the residence decision stays in one place.
#[cfg(not(feature = "flash-image"))]
fn owned_cil(bytes: Vec<u8>) -> RawCil {
    bytes.into_boxed_slice()
}

/// See the non-XIP form above.
#[cfg(feature = "flash-image")]
fn owned_cil(bytes: Vec<u8>) -> RawCil {
    RawCil::Ram(bytes.into_boxed_slice())
}
