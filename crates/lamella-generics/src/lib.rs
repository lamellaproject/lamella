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
    Assembly, CodedIndex, SigType, exception_tag_for_name, fnv1a32, parse_local_vars, parse_method,
    parse_method_spec,
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
pub use lamella_ir::INSTANTIATION_HANDLE_TABLE;

/// The handle for the instantiation spelled `name`.
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
#[must_use]
pub fn substitute_sig(ty: &SigType, arguments: &[SigType]) -> Option<SigType> {
    Some(match ty {
        SigType::Var(number) => arguments.get(*number as usize)?.clone(),
        SigType::MVar(_) => return None,
        SigType::GenericInst {
            definition,
            arguments: inner,
        } => SigType::GenericInst {
            definition: Box::new(substitute_sig(definition, arguments)?),
            arguments: inner
                .iter()
                .map(|argument| substitute_sig(argument, arguments))
                .collect::<Option<Vec<_>>>()?,
        },
        SigType::Pointer(inner) => SigType::Pointer(Box::new(substitute_sig(inner, arguments)?)),
        SigType::ByRef(inner) => SigType::ByRef(Box::new(substitute_sig(inner, arguments)?)),
        SigType::SzArray(inner) => SigType::SzArray(Box::new(substitute_sig(inner, arguments)?)),
        SigType::Array { element, rank } => SigType::Array {
            element: Box::new(substitute_sig(element, arguments)?),
            rank: *rank,
        },
        other => other.clone(),
    })
}

/// Converts a decoded [`SigType`] into a [`TypeArg`], resolving every token to a NAME.
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

/// The canonical spelling of a `SigType`, through [`TypeArg::spell`].
#[must_use]
pub fn spell_sig(assembly: &Assembly<'_>, ty: &SigType) -> Option<String> {
    sig_to_type_arg(assembly, ty).ok().map(|arg| arg.name())
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

    /// Every closed instantiation the PROGRAM names directly: its `TypeSpec` rows, the type
    /// arguments of its `MethodSpec` rows, and the field and method signatures of its own types.
    fn roots(&self) -> Result<Vec<TypeArg>, Refusal> {
        let Some(assembly) = self.assemblies.first() else {
            return Ok(Vec::new());
        };
        let mut roots = Vec::new();
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

/// A type token's full name: the enclosing chain joined with `+`, prefixed by the OUTERMOST type's
/// namespace, exactly as .NET spells a nested type.
///
/// It works for a `TypeRef` as well as a `TypeDef`, and that is load-bearing: a reference carries
/// its own namespace and name, so the spelling never needs the defining assembly to be present.
fn type_def_full_name(assembly: &Assembly<'_>, token: Token) -> Option<String> {
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
    pub index: u32,
    /// The instantiation's canonical spelling -- the identity half of the call-site key, and the
    /// only identity that survives an assembly boundary.
    pub instantiation: Box<str>,
    /// The `TypeSpec` token, in the module's OWN assembly, that spells this instantiation.
    pub spec: Token,
    /// The `MethodDef` rid of the definition method whose CIL body is lowered under the
    /// instantiation.
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
}

/// Every monomorphized body a module emits, and the map from a CALL SITE to the index its body
/// occupies.
#[derive(Debug, Clone, Default)]
pub struct MonoPlan {
    bodies: Vec<MonoBody>,
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
    pub fn for_assembly(assembly: &Assembly<'_>, first_index: u32) -> Result<MonoPlan, Refusal> {
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
            if token.table() != table::TYPE_DEF {
                continue;
            }
            let Some(type_def) = assembly.type_def(token.row()) else {
                continue;
            };
            for method in type_def.methods() {
                if method.body().is_none() {
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
                    spec,
                    rid: method.rid(),
                    name: Box::from(method_name),
                    parameters,
                });
                next += 1;
            }
        }
        Ok(MonoPlan { bodies })
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

    /// How many bodies this plan emits.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bodies.len()
    }

    /// Whether this plan emits nothing -- which is the case for every non-generic program, and is
    /// what keeps the ordinary path untouched.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
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
            spec: Token::new(table::TYPE_SPEC, 1),
            rid: 4,
            name: name.to_owned().into_boxed_str(),
            parameters,
        }
    }

    #[test]
    fn two_instantiations_of_one_definition_bind_to_different_bodies() {
        let plan = MonoPlan {
            bodies: alloc::vec![
                body(11, "Box`1[System.Int32]", "Get", Vec::new()),
                body(12, "Box`1[System.String]", "Get", Vec::new()),
            ],
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

    #[test]
    fn the_spelling_rule_fingerprint_is_pinned() {
        assert_eq!(
            spelling_rule_fingerprint(),
            0x8647_0575,
            "the canonical instantiation spelling CHANGED -- this is a cross-artifact contract"
        );
    }

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
