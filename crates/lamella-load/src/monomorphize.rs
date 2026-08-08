//! Bake-time monomorphization: the SUBSTITUTION AND EMISSION half.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use lamella_cil::Opcode;
use lamella_cil_runtime::module::RawCil;
use lamella_cil_runtime::{MethodId, Module, TypeId, Value};
use lamella_metadata::{Assembly, SigType};
use lamella_token::Token;

use super::{
    FieldNameIndex, MEMBER_REF, METHOD_SPEC, TYPE_DEF, TYPE_REF, TYPE_SPEC, TypeNameIndex,
    arg_count, cast_elem_of_sig, default_field_value, encode_sig_type, full_type_name,
    type_name_key,
};

/// The metadata table id the rewritten tokens live in.
///
/// ECMA-335 assigns table ids up to `0x2C` and the heap-token ids `0x70`..`0x72`, so `0x7E` names
/// nothing a reader can produce. That matters: a synthetic token must be one no real row could
/// collide with, since the tables it keys are shared with the assembly's own tokens.
const SYNTHETIC_TABLE: u8 = 0x7E;

/// The closed instantiation set `program` requires, collected and spelled by the SHARED collector
/// both tiers consume.
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
            let (definition, arguments) = rows
                .iter()
                .find(|(name, _)| name.as_str() == found.name.as_ref())
                .map(|(_, parts)| parts.clone())?;
            Some(Instantiation {
                definition,
                arguments,
                name: found.name.into_string(),
            })
        })
        .collect()
}

/// One closed instantiation to lower.
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

/// Why one instantiation, or one token inside one, could not be lowered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The definition named by the set is not a `TypeDef` of this assembly. The set is built from
    /// one assembly's roots and walked across all of them, so this is a real possibility and not a
    /// malformed input -- but a body cannot be duplicated out of metadata that is not here.
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
    /// The definition's `TypeDef` row (1-based).
    def_row: u32,
    /// The slot each of the definition's own instance fields lands in, by declaration order.
    own_field_slots: Vec<u32>,
    /// The `MethodId` of each of the definition's methods, by declaration order. `None` where the
    /// definition's method has no body to duplicate (abstract, or runtime-supplied).
    methods: Vec<Option<MethodId>>,
}

/// A definition's method as this pass needs to see it.
struct DefMethod<'pe> {
    name: String,
    /// The parameter types AS DECLARED -- still mentioning `!n`, which is also the form the call
    /// site's `MemberRef` carries. Matching happens on this form; the DISPATCH KEY is built from
    /// its substitution.
    params: Vec<SigType>,
    arg_count: u16,
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
    assembly: &Assembly<'pe>,
    asm: u8,
    type_offset: usize,
    type_index: &TypeNameIndex,
    field_index: &FieldNameIndex,
    materialize: super::CilMaterializer<'pe>,
    instantiations: &[Instantiation],
) -> Lowering {
    let mut lowering = Lowering {
        types: Vec::new(),
        refusals: Vec::new(),
    };
    if instantiations.is_empty() {
        return lowering;
    }

    let mut definition_rows: BTreeMap<String, u32> = BTreeMap::new();
    let mut row = 0u32;
    for type_def in assembly.type_defs() {
        row += 1;
        if let Some(name) = type_def.name() {
            definition_rows.entry(full_type_name(name)).or_insert(row);
        }
    }

    let mut emitted: Vec<(usize, Emitted)> = Vec::new();
    for (index, want) in instantiations.iter().enumerate() {
        let Some(&def_row) = definition_rows.get(&want.definition) else {
            lowering.refusals.push(Refusal::DefinitionNotHere {
                definition: want.definition.clone(),
            });
            continue;
        };
        let type_id = module.add_type(Vec::new());
        module.bind_type_full_name(type_id, want.name.clone());
        lowering.types.push((want.name.clone(), type_id));
        emitted.push((
            index,
            Emitted {
                type_id,
                def_row,
                own_field_slots: Vec::new(),
                methods: Vec::new(),
            },
        ));
    }

    for (index, entry) in &mut emitted {
        let want = &instantiations[*index];
        let Some(type_def) = assembly.type_def(entry.def_row) else {
            continue;
        };
        let base = base_type_of(
            module,
            assembly,
            type_offset,
            type_index,
            &definition_rows,
            type_def.extends(),
        );
        module.set_type_base(entry.type_id, base);
        let mut defaults = base
            .and_then(|base| module.type_field_defaults(base))
            .unwrap_or_default();
        let mut refused = false;
        for field in type_def.fields() {
            if field.is_static() {
                if !field.is_literal() {
                    lowering.refusals.push(Refusal::StaticFieldNotSeparated {
                        instantiation: want.name.clone(),
                        field: field.name().unwrap_or("").into(),
                    });
                    refused = true;
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
                assembly,
                &substituted,
                &field_index.enum_zeros,
            ));
        }
        if refused {
            continue;
        }
        module.set_type_field_defaults(entry.type_id, defaults);
    }

    let mut definition_methods: BTreeMap<u32, Vec<DefMethod<'pe>>> = BTreeMap::new();
    for (_, entry) in &emitted {
        definition_methods
            .entry(entry.def_row)
            .or_insert_with(|| read_definition_methods(assembly, entry.def_row));
    }

    let mut next_synthetic_row: u32 = 1;
    for position in 0..emitted.len() {
        let (index, def_row, type_id) = {
            let (index, entry) = &emitted[position];
            (*index, entry.def_row, entry.type_id)
        };
        let want = &instantiations[index];
        let methods = definition_methods.get(&def_row).map(Vec::as_slice).unwrap_or(&[]);
        let mut ids: Vec<Option<MethodId>> = Vec::with_capacity(methods.len());
        for method in methods {
            let Some(raw) = method.raw else {
                ids.push(None);
                continue;
            };
            let plan = match plan_rewrite(assembly, raw) {
                Ok(plan) => plan,
                Err(()) => {
                    lowering.refusals.push(Refusal::BodyUnreadable {
                        instantiation: want.name.clone(),
                        method: method.name.clone(),
                    });
                    ids.push(None);
                    continue;
                }
            };
            let mut body_refused = false;
            let cil = if plan.is_empty() {
                materialize(raw)
            } else {
                let mut bytes = raw.to_vec();
                for site in &plan {
                    let synthetic = Token::new(SYNTHETIC_TABLE, next_synthetic_row);
                    next_synthetic_row += 1;
                    match bind_open_token(
                        module,
                        assembly,
                        asm,
                        type_offset,
                        type_index,
                        field_index,
                        instantiations,
                        &emitted,
                        &definition_methods,
                        want,
                        site,
                        synthetic,
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
                owned_cil(bytes)
            };
            if body_refused {
                ids.push(None);
                continue;
            }
            let id = module.add_method(asm, cil, method.arg_count);
            module.set_method_type(id, type_id);
            #[cfg(feature = "debug-names")]
            module.set_method_debug(
                id,
                alloc::format!("{}.{}", want.name, method.name),
                Vec::new(),
            );
            ids.push(Some(id));
        }
        emitted[position].1.methods = ids;
    }

    for (index, entry) in &emitted {
        let want = &instantiations[*index];
        let methods = definition_methods.get(&entry.def_row).map(Vec::as_slice).unwrap_or(&[]);
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
            let Some(key) = substituted_sig_key(assembly, &method.name, &method.params, &want.arguments)
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

    bind_closed_type_specs(module, assembly, asm, instantiations, &emitted);
    bind_closed_member_refs(
        module,
        assembly,
        asm,
        instantiations,
        &emitted,
        &definition_methods,
        &mut lowering,
    );

    lowering
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
                is_static: method.is_static(),
                is_virtual: method.flags() & super::METHOD_VIRTUAL != 0,
                newslot: method.flags() & super::METHOD_NEWSLOT != 0,
                raw: method.body_and_bytes().map(|(_, raw)| raw),
            }
        })
        .collect()
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
fn plan_rewrite<'pe>(assembly: &Assembly<'pe>, raw: &[u8]) -> Result<Vec<OpenSite>, ()> {
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
        if !token_is_open(assembly, token) {
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
fn token_is_open<'pe>(assembly: &Assembly<'pe>, token: Token) -> bool {
    match token.table() {
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

/// Whether a signature mentions a type parameter of either kind, anywhere inside it.
fn mentions_parameter(sig: &SigType) -> bool {
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

/// `sig` with `!n` replaced by `arguments[n]`.
fn substitute(sig: &SigType, arguments: &[SigType]) -> Option<SigType> {
    Some(match sig {
        SigType::Var(number) => arguments.get(*number as usize)?.clone(),
        SigType::MVar(_) => return None,
        SigType::GenericInst {
            definition,
            arguments: inner,
        } => SigType::GenericInst {
            definition: Box::new(substitute(definition, arguments)?),
            arguments: inner
                .iter()
                .map(|argument| substitute(argument, arguments))
                .collect::<Option<Vec<_>>>()?,
        },
        SigType::Pointer(inner) => SigType::Pointer(Box::new(substitute(inner, arguments)?)),
        SigType::ByRef(inner) => SigType::ByRef(Box::new(substitute(inner, arguments)?)),
        SigType::SzArray(inner) => SigType::SzArray(Box::new(substitute(inner, arguments)?)),
        SigType::Array { element, rank } => SigType::Array {
            element: Box::new(substitute(element, arguments)?),
            rank: *rank,
        },
        other => other.clone(),
    })
}

/// The dispatch key a member of an instantiation answers to: its name and its parameter types with
/// the instantiation's arguments substituted in.
fn substituted_sig_key<'pe>(
    assembly: &Assembly<'pe>,
    name: &str,
    params: &[SigType],
    arguments: &[SigType],
) -> Option<String> {
    let mut key = alloc::format!("{name}|");
    for param in params {
        key.push_str(&encode_sig_type(assembly, &substitute(param, arguments)?));
        key.push(',');
    }
    Some(key)
}

/// The zero value one substituted field signature takes.
fn default_field_value_substituted<'pe>(
    assembly: &Assembly<'pe>,
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
    default_field_value(Some(sig.clone()))
}

/// The `TypeId` a definition's `extends` token names, if this load can see it.
fn base_type_of<'pe>(
    module: &Module,
    assembly: &Assembly<'pe>,
    type_offset: usize,
    type_index: &TypeNameIndex,
    _definition_rows: &BTreeMap<String, u32>,
    extends: Token,
) -> Option<TypeId> {
    let _ = module;
    match extends.table() {
        TYPE_DEF => {
            let index = (extends.row() as usize).checked_sub(1)?;
            Some((type_offset + index) as TypeId)
        }
        TYPE_REF => assembly
            .type_token_name(extends)
            .and_then(|name| type_index.get(&type_name_key(name)).copied()),
        _ => None,
    }
}

/// Finds the instantiation of `definition` with exactly `arguments`.
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

/// Binds one rewritten token to what it means for THIS instantiation.
#[allow(clippy::too_many_arguments)]
fn bind_open_token<'pe>(
    module: &mut Module,
    assembly: &Assembly<'pe>,
    asm: u8,
    type_offset: usize,
    type_index: &TypeNameIndex,
    field_index: &FieldNameIndex,
    instantiations: &[Instantiation],
    emitted: &[(usize, Emitted)],
    definition_methods: &BTreeMap<u32, Vec<DefMethod<'pe>>>,
    want: &Instantiation,
    site: &OpenSite,
    synthetic: Token,
) -> Result<(), Refusal> {
    let unbound = || Refusal::UnboundAfterSubstitution {
        instantiation: want.name.clone(),
        token: site.token.0,
    };
    match site.token.table() {
        METHOD_SPEC => Err(Refusal::MethodTypeParameter {
            instantiation: want.name.clone(),
            token: site.token.0,
        }),
        TYPE_SPEC => {
            let sig = assembly
                .type_spec_signature(site.token)
                .ok_or_else(unbound)?;
            let substituted = substitute(&sig, &want.arguments).ok_or_else(|| {
                Refusal::MethodTypeParameter {
                    instantiation: want.name.clone(),
                    token: site.token.0,
                }
            })?;
            bind_type_operand(
                module,
                assembly,
                asm,
                type_offset,
                type_index,
                instantiations,
                emitted,
                want,
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
            let definition_name = assembly
                .type_token_name(definition_token)
                .map(full_type_name)
                .ok_or_else(unbound)?;
            let substituted: Vec<SigType> = arguments
                .iter()
                .map(|argument| substitute(argument, &want.arguments))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| Refusal::MethodTypeParameter {
                    instantiation: want.name.clone(),
                    token: site.token.0,
                })?;
            let target = find_instantiation(instantiations, emitted, &definition_name, &substituted)
                .ok_or_else(|| Refusal::InstantiationNotInSet {
                    instantiation: want.name.clone(),
                    wanted: definition_name.clone(),
                })?;
            let (target_index, target_entry) = &emitted[target];
            let target_want = &instantiations[*target_index];
            let name = member.name().unwrap_or("");
            if member.is_field() {
                let slot = definition_field_position(assembly, target_entry.def_row, name)
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
                .get(&target_entry.def_row)
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
            if let Some(key) = substituted_sig_key(
                assembly,
                name,
                &methods[position].params,
                &target_want.arguments,
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
    type_offset: usize,
    type_index: &TypeNameIndex,
    instantiations: &[Instantiation],
    emitted: &[(usize, Emitted)],
    want: &Instantiation,
    site: &OpenSite,
    synthetic: Token,
    substituted: &SigType,
) -> Result<(), Refusal> {
    let unbound = || Refusal::UnboundAfterSubstitution {
        instantiation: want.name.clone(),
        token: site.token.0,
    };
    let _ = type_offset;
    module.bind_cast_elem(asm, synthetic, cast_elem_of_sig(asm, substituted));
    if matches!(site.opcode, Opcode::Newarr) {
        module.bind_array_default(asm, synthetic, default_field_value(Some(substituted.clone())));
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
            let definition_name = assembly
                .type_token_name(definition_token)
                .map(full_type_name)
                .ok_or_else(unbound)?;
            let target = find_instantiation(instantiations, emitted, &definition_name, arguments)
                .ok_or_else(|| Refusal::InstantiationNotInSet {
                    instantiation: want.name.clone(),
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
            module.bind_type_name(asm, synthetic, primitive_display_name(other).into());
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

/// The simple name a primitive `SigType` displays under, for `typeof(T).Name`.
fn primitive_display_name(sig: &SigType) -> &'static str {
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
        let Some(name) = assembly.type_token_name(definition_token).map(full_type_name) else {
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
    }
}

/// Binds every `MemberRef` reached through a CLOSED instantiation to the member of the type this
/// pass made, and withdraws that token's `UnloweredGeneric` mark -- but ONLY once it is bound.
fn bind_closed_member_refs<'pe>(
    module: &mut Module,
    assembly: &Assembly<'pe>,
    asm: u8,
    instantiations: &[Instantiation],
    emitted: &[(usize, Emitted)],
    definition_methods: &BTreeMap<u32, Vec<DefMethod<'pe>>>,
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
        let Some(definition_name) = assembly.type_token_name(definition_token).map(full_type_name)
        else {
            continue;
        };
        let Some(target) = find_instantiation(instantiations, emitted, &definition_name, &arguments)
        else {
            continue;
        };
        let (index, entry) = &emitted[target];
        let want = &instantiations[*index];
        let name = member.name().unwrap_or("");
        if member.is_field() {
            let Some(slot) = definition_field_position(assembly, entry.def_row, name)
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
            .get(&entry.def_row)
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
        let Some(id) = entry.methods.get(position).copied().flatten() else {
            lowering.refusals.push(Refusal::UnboundAfterSubstitution {
                instantiation: want.name.clone(),
                token: token.0,
            });
            continue;
        };
        module.bind_token(asm, token, id);
        if let Some(key) =
            substituted_sig_key(assembly, name, &methods[position].params, &want.arguments)
        {
            let count = u16::try_from(params.len() + 1).unwrap_or(u16::MAX);
            module.bind_call_target(asm, token, key, count);
        }
        module.clear_unlowered_generic(asm, token);
    }
}

/// A body whose bytes were REWRITTEN: it must be owned, because it no longer matches anything in
/// the PE and there is nothing left to borrow.
#[cfg(not(feature = "flash-image"))]
fn owned_cil(bytes: Vec<u8>) -> RawCil {
    bytes.into_boxed_slice()
}

/// See the non-XIP form above.
#[cfg(feature = "flash-image")]
fn owned_cil(bytes: Vec<u8>) -> RawCil {
    RawCil::Ram(bytes.into_boxed_slice())
}
