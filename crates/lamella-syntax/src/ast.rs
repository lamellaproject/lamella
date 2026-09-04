//! The syntax tree the parser builds from the token stream.

use crate::span::Span;
use crate::token::{IntegerSuffix, RealSuffix};
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

/// An expression: a [`ExprKind`] together with the source [`Span`] it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    /// What kind of expression this is, with its children.
    pub kind: ExprKind,
    /// The byte range the expression covers in the source.
    pub span: Span,
}

impl Expr {
    /// Creates an expression node of `kind` covering `span`.
    #[must_use]
    pub fn new(kind: ExprKind, span: Span) -> Expr {
        Expr { kind, span }
    }

    /// Creates a simple-name expression that was NOT written verbatim -- the shape every
    /// desugaring wants, since a name a compiler synthesized has no `@` to record.
    #[must_use]
    pub fn name(name: impl Into<Box<str>>, span: Span) -> Expr {
        Expr::new(
            ExprKind::Name {
                name: name.into(),
                verbatim: false,
            },
            span,
        )
    }

    /// This expression's simple-name text WHEN IT MAY BE READ AS A CONTEXTUAL KEYWORD -- `None`
    /// for anything that is not a bare name, and `None` for a verbatim one, which 9.4.2 forces
    /// back to an ordinary identifier.
    ///
    /// **THE EXPRESSION-POSITION HALF OF THE RULE `Parser::current_contextual_keyword` SPENDS FOR
    /// TOKENS.** The parser's sites see a token and can read its `@` directly; a binder site sees
    /// an [`Expr`] whose name has already had the `@` dropped, so the flag has to travel on the
    /// node and be spent here rather than at whichever site notices first. `@nameof(x)` is a call
    /// to a method called `nameof` -- csc reports `CS0103` when no such method exists, measured --
    /// and a site comparing the text alone reads it as the operator and accepts an invalid
    /// program in silence.
    #[must_use]
    pub fn contextual_keyword(&self) -> Option<&str> {
        match &self.kind {
            ExprKind::Name {
                name,
                verbatim: false,
            } => Some(name),
            _ => None,
        }
    }
}

/// One piece of an [`ExprKind::InterpolatedString`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterpolationPart {
    /// Literal text, escapes and `{{`/`}}` already decoded. Adjacent literals never occur, so a
    /// piece is always maximal -- which matters because the `String.Concat` overload the binder
    /// picks is chosen by COUNTING the pieces.
    Literal(Box<[u16]>),
    /// A `{ expression [, alignment] [: format] }` hole.
    Hole {
        /// The interpolated expression.
        expression: Box<Expr>,
        /// The alignment after a `,`, as an expression -- csc binds it as a constant one, so
        /// `$"{n,99999999999}"` draws `CS0266`/`CS0150` rather than a lexical complaint.
        alignment: Option<Box<Expr>>,
        /// The format specifier after a `:`, taken literally. Never `Some("")`.
        format: Option<Box<str>>,
    },
}

/// Which position a `ref` operand was written in, for the sake of the diagnostic a BAD operand
/// draws there.
///
/// **ONE OPERAND, FOUR POSITIONS, THREE DIFFERENT CODES -- MEASURED AGAINST csc AT 7.3, NOT
/// REASONED.** The same non-ref property `P`:
///
/// | position | code |
/// |---|---|
/// | `ref int r = ref P;` and `M(ref P)` | `CS0206` |
/// | `r = ref P;` (ref REASSIGNMENT) | `CS1510` |
/// | `return ref P;` | `CS8156` |
///
/// and a `readonly` field in the same three: `CS0192`, `CS0191`, `CS8160`.
///
/// **THE POSITION RIDES THE NODE BECAUSE ONLY THE PARSER KNOWS IT.** The binder builds one
/// `BoundExprKind::Ref` for every position -- which is right, since they all mean *the address of*
/// -- so a check written there cannot tell them apart, and one written at each consumer is a list
/// of sites for the next position to be forgotten from. A first cut did exactly that and made
/// `return ref P` report `CS0206` where csc reports `CS8156`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefPosition {
    /// A `ref`/`out` ARGUMENT at a call site, and a `ref` LOCAL's initializer. csc answers these
    /// two identically at every operand tested.
    Argument,
    /// The right-hand side of a ref REASSIGNMENT, `r = ref e` (C# 7.3).
    Reassignment,
    /// `return ref e;`.
    Return,
}

/// The kind of an [`Expr`], with any child expressions (ECMA-334 1st ed, 14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    /// A literal value (14.5.1): the lexer decoded it; this carries the result.
    Literal(Literal),
    /// A simple name (14.5.2): a bare identifier, its `@` prefix already removed.
    Name {
        /// The identifier, without the `@` of a verbatim spelling -- `@nameof` is `nameof` here,
        /// because that is the identifier it denotes (9.4.2) and what it must bind to.
        name: Box<str>,
        /// Whether the `@` was written. **THE NAME ALONE CANNOT SAY**, and an expression-position
        /// contextual keyword needs to know: `@nameof(x)` is a call to something called `nameof`
        /// and never the operator. Ask [`Expr::contextual_keyword`] rather than reading this.
        verbatim: bool,
    },
    /// A predefined type in expression position (14.5.4): the left side of a
    /// static member access such as `int.Parse`. Binding rejects it anywhere a
    /// value, rather than a type name, is required.
    PredefinedType(PredefinedType),
    /// A CONSTRUCTED GENERIC TYPE in expression position: the left side of a static member
    /// access such as `Box<int>.Count`.
    ///
    /// It is its own node rather than an `Invocation` with no arguments because the `<` here is
    /// ambiguous with a comparison and only the FOLLOWING token resolves it. The parser commits to
    /// this reading when a `.` follows the closing `>`, which is one of the followers the
    /// disambiguation rule names; `a < b > .c` is not a legal comparison, so nothing is taken away
    /// from the operator parser by claiming it.
    ConstructedType {
        /// The type's name -- a simple name, or a member-access chain for a qualified one.
        name: Box<Expr>,
        /// The type arguments between the angle brackets. Never empty: `Box<>` is not a
        /// type-argument list and the parser does not commit to one.
        type_arguments: Vec<TypeRef>,
    },
    /// The `this` access (14.5.7).
    This,
    /// A `base` access (14.5.8): valid only as `base.member` or `base[args]`.
    Base,
    /// A parenthesized expression (14.5.3): the parentheses group, they are not
    /// part of the value, so the inner expression is kept directly.
    Parenthesized(Box<Expr>),
    /// A member access `receiver.name` (14.5.4).
    MemberAccess {
        /// The expression whose member is named.
        receiver: Box<Expr>,
        /// The accessed member's name.
        name: Box<str>,
    },
    /// A NULL-CONDITIONAL ACCESS `receiver?.name`, `receiver?[index]` and everything chained
    /// after it (C# 6.0): the receiver is evaluated once, and the whole of `access` is skipped
    /// when it is null.
    ///
    /// **`access` IS THE WHOLE REMAINING CHAIN, NOT THE ONE MEMBER, AND THAT IS THE SEMANTICS
    /// RATHER THAN A CONVENIENCE.** `a?.B.C.D` evaluates NONE of `B`, `C` or `D` when `a` is null
    /// -- measured: a receiver-null `nul?.Inner.Val` touches neither property, where a per-member
    /// reading would evaluate `.Inner` on null and throw. So the parser hands the rest of the
    /// postfix chain to this node rather than wrapping this node in it.
    ///
    /// `access` is rooted at a [`ExprKind::ConditionalReceiver`] standing for the tested value.
    ConditionalAccess {
        /// The expression tested for null, evaluated exactly once.
        receiver: Box<Expr>,
        /// The chain applied when it is not null, rooted at [`ExprKind::ConditionalReceiver`].
        access: Box<Expr>,
    },
    /// The placeholder at the root of a [`ExprKind::ConditionalAccess`]'s `access`: the receiver's
    /// value, already tested and already unwrapped. It never appears anywhere else, and binding
    /// one outside a conditional access is a compiler defect rather than a source error.
    ConditionalReceiver,
    /// An invocation `receiver(arguments)` (14.5.5).
    Invocation {
        /// The expression being invoked.
        receiver: Box<Expr>,
        /// The explicit type arguments at this call site -- the `int` of `Identity<int>(x)` --
        /// empty for an ordinary call and for one whose arguments are inferred.
        ///
        /// These belong to the CALL SITE, not to the method: `Identity<int>(a)` and
        /// `Identity<string>(b)` are two sites over one declaration, and each emits its own
        /// `MethodSpec`. A site that loses them still binds -- to the OPEN method, with `!!0`
        /// unsubstituted -- so dropping this field is silent rather than a compile error.
        type_arguments: Vec<TypeRef>,
        /// The argument expressions, in order.
        arguments: Vec<Expr>,
    },
    /// An element access `receiver[arguments]` (14.5.6).
    ElementAccess {
        /// The expression being indexed.
        receiver: Box<Expr>,
        /// The index argument expressions, in order.
        arguments: Vec<Expr>,
    },
    /// A prefix unary operation, including pre-increment and pre-decrement (14.6).
    Unary {
        /// The operator applied.
        operator: UnaryOperator,
        /// The operand it applies to.
        operand: Box<Expr>,
    },
    /// An `await` expression `await unary-expression` (ECMA-334 5th ed, 12.8.8).
    ///
    /// `await` is not a keyword: inside an async method every use of the bare word is this node
    /// (the word is reserved there, 12.8.8.1), and OUTSIDE one the parser builds it only for the
    /// measured operator shapes -- `await` followed by an identifier, a literal, or `new` in an
    /// expression position -- reporting CS4033, so `await(1)` stays a call and `await + 1` stays
    /// an identifier plus a binary operator, exactly as csc reads them.
    Await(Box<Expr>),
    /// A `ref`/`out` argument (17.5.1): the address of a variable, passed to a byref
    /// parameter. `out` additionally means the callee assigns the variable.
    ///
    /// **AND THE `ref` OF `return ref e;` (C# 7.0), WHICH IS THE SAME NODE BECAUSE IT DENOTES THE
    /// SAME THING**: the address of a variable rather than its value. The two positions consume it
    /// identically -- the binder produces one `BoundExprKind::Ref` for both, and the emitter takes
    /// an address for both through one function -- so a second variant would be two spellings of
    /// one meaning, and the second one is where a case gets forgotten. What differs is the
    /// DIAGNOSTIC each position reports, and the position decides it rather than the node:
    /// a `ref` return in a by-value method is `CS8149`, which no argument can be.
    ///
    /// `out` is always `false` in the return position -- `return out e;` is not a form.
    RefArgument {
        /// `true` for `out`, `false` for `ref`.
        out: bool,
        /// Which of the four positions this `ref` was written in. The node is the same in all
        /// four; the DIAGNOSTIC for a bad operand is not, and only the parser knows which site it
        /// is. See [`RefPosition`].
        position: RefPosition,
        /// The variable whose address is passed.
        operand: Box<Expr>,
    },
    /// A postfix `++` or `--` (14.5.9).
    PostfixUnary {
        /// Whether the operator increments or decrements.
        operator: PostfixOperator,
        /// The operand it applies to.
        operand: Box<Expr>,
    },
    /// A binary operation (14.7 through 14.12).
    Binary {
        /// The operator applied.
        operator: BinaryOperator,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
    },
    /// A null-coalescing `left ?? right` (C# 2.0; ECMA-334 4th ed 14.13): `left` when it is not
    /// null, otherwise `right`.
    ///
    /// **ITS OWN VARIANT RATHER THAN A [`BinaryOperator`], BECAUSE IT IS NOT AN OPERATOR ON
    /// VALUES.** It short-circuits, it is not overloadable (14.13 says so outright), it is the one
    /// RIGHT-associative binary spelling in the language, and its result type comes from a
    /// conversion between the two operand types rather than from a promotion. A variant on the
    /// operator enum would have carried none of that and would have inherited the overload lookup
    /// every other member of that enum gets.
    NullCoalescing {
        /// The left operand -- the value used when it is not null.
        left: Box<Expr>,
        /// The right operand -- evaluated only when the left one is null.
        right: Box<Expr>,
    },
    /// A THROW EXPRESSION, `throw e` in expression position (C# 7.0).
    ///
    /// **It is parsed far more widely than it is PERMITTED, and that is csc's shape rather than a
    /// looseness here.** The parser admits one wherever an expression is parsed at null-coalescing
    /// precedence or lower, which is why `f(throw e)` and `(throw e)` reach the binder at all --
    /// csc refuses both as `CS8115`, a message about the CONTEXT, and it can only say that about
    /// something it parsed. As the operand of a binary operator it is not admitted by either
    /// compiler, and both say `CS1525` there instead.
    Throw(Box<Expr>),
    /// A LAMBDA EXPRESSION (C# 3.0): `x => x + 1`, `(a, b) => a * b`, `() => 0`,
    /// `(int x) => x`, `x => { return x; }`.
    ///
    /// **IT HAS NO TYPE OF ITS OWN AND THAT IS THE WHOLE DIFFICULTY.** A lambda is converted to a
    /// DELEGATE type supplied by its context (14.5.11), and until that context is known neither its
    /// parameter types nor its return type exist -- so the parser records the shape and nothing
    /// else, and the binder types it against the target. An IMPLICITLY typed parameter list
    /// (`x => ...`) carries a `TypeRef` only when the source wrote one.
    Lambda {
        /// The parameters as written. A parameter whose type the source omitted has `ty: None`,
        /// and the binder fills it from the target delegate's signature.
        ///
        /// **ALL OR NONE (14.5.11).** C# does not permit a parameter list that mixes explicit
        /// and implicit types, and the check belongs to the binder rather than here: the parser's
        /// answer is what was written.
        parameters: Vec<LambdaParameter>,
        /// The body: an expression (`x => x + 1`) or a block (`x => { return x + 1; }`).
        body: Box<LambdaBody>,
        /// Whether the list was written parenthesized. `x => ...` and `(x) => ...` are the same
        /// lambda; the flag exists because a diagnostic about the list points at different spans.
        parenthesized: bool,
    },
    /// A conditional `condition ? when_true : when_false` (14.13).
    Conditional {
        /// The condition tested.
        condition: Box<Expr>,
        /// The value when the condition is true.
        when_true: Box<Expr>,
        /// The value when the condition is false.
        when_false: Box<Expr>,
    },
    /// An assignment, simple or compound (14.14).
    Assignment {
        /// Which assignment operator was used.
        operator: AssignmentOperator,
        /// The assignment target.
        target: Box<Expr>,
        /// The value assigned.
        value: Box<Expr>,
    },
    /// An INTERPOLATED STRING (C# 6.0): `$"a{b}c"` and its verbatim form, as literal pieces and
    /// holes. The scanner already decoded the escapes and split the holes out; what survives to
    /// here is the SHAPE, because the shape is what decides the lowering and the binder is the
    /// first thing that knows the holes' types.
    InterpolatedString(Vec<InterpolationPart>),
    /// A `typeof` expression (14.5.11): `typeof ( type )`.
    TypeOf(TypeRef),
    /// A `sizeof` expression (unsafe, III.4.25): `sizeof ( type )`. Its value is the type's
    /// byte size; for a struct it is the `sizeof` opcode over the shared layout.
    SizeOf(TypeRef),
    /// A `default` expression (C# 2.0): `default ( type )` -- the type's default value. `null` for
    /// a reference type, zero for a numeric, and the all-zero value for a struct.
    ///
    /// **It is the only way to write the zero of a TYPE PARAMETER**, whose default is not
    /// spellable otherwise: `T` may be closed over a reference type (where the answer is `null`)
    /// or a value type (where it is not), so no literal covers both and the choice has to be made
    /// where `T` is known.
    DefaultValue(TypeRef),
    /// A `stackalloc` expression (unsafe): `stackalloc T [ count ]`. Allocates
    /// `count * sizeof(T)` bytes on the call stack and yields a `T*` to the start.
    StackAlloc {
        /// The element type.
        element: TypeRef,
        /// The element count.
        count: Box<Expr>,
    },
    /// A pointer indirection (unsafe): the prefix `* operand`, reading or writing the value
    /// the pointer addresses (its element type). An lvalue when assigned.
    Dereference(Box<Expr>),
    /// The address-of operator (unsafe, 18.5.4): the prefix `& operand`, yielding a `T*` to a
    /// fixed variable (a local, value parameter, field, or array element). The inverse of
    /// [`Dereference`].
    AddressOf(Box<Expr>),
    /// A `checked ( expression )` (14.5.12), forcing overflow checking on.
    Checked(Box<Expr>),
    /// An `unchecked ( expression )` (14.5.12), forcing overflow checking off.
    Unchecked(Box<Expr>),
    /// A `__makeref ( variable )`: csc's typed-reference constructor (parsed only under
    /// [`crate::lexer::LexOptions::typedref`]). The operand is a variable; the result is a
    /// `System.TypedReference` pairing its address with its static type. Lowers to `mkrefany`.
    MakeRef(Box<Expr>),
    /// A `__reftype ( reference )`: the runtime `System.Type` of a typed reference. Lowers to
    /// `refanytype` followed by `Type.GetTypeFromHandle`.
    RefType(Box<Expr>),
    /// A `__refvalue ( reference , type )`: the referent of a typed reference, viewed as `type`
    /// and usable as an lvalue. Lowers to `refanyval <type>` (then a load, or a store when it is
    /// an assignment target).
    RefValue {
        /// The typed-reference operand.
        reference: Box<Expr>,
        /// The asserted referent type.
        target: TypeRef,
    },
    /// A bare `__arglist` inside a vararg member's body (parsed only under
    /// [`crate::lexer::LexOptions::typedref`]): the handle to the current method's variable
    /// arguments, of type `System.RuntimeArgumentHandle`. Lowers to the `arglist` opcode.
    /// Outside a vararg member it is CS0190.
    ArgListHandle,
    /// An `__arglist ( argument, ... )` at a call site: the variable arguments passed past a
    /// vararg member's fixed parameters. Legal only as the final argument of a call or object
    /// creation whose target is a vararg member (CS0226 elsewhere); the argument types ride in
    /// the call-site signature after the sentinel.
    ArgListCall(Vec<Expr>),
    /// An `is` or `as` type test (14.9.9, 14.9.10): the operand against a type.
    TypeTest {
        /// Whether this is `is` or `as`.
        operation: TypeTestOperation,
        /// The expression being tested or converted.
        operand: Box<Expr>,
        /// The type tested against.
        target: TypeRef,
    },
    /// A cast `( type ) operand` (14.6.6).
    Cast {
        /// The type cast to.
        target: TypeRef,
        /// The expression being cast.
        operand: Box<Expr>,
    },
    /// An object (or delegate) creation `new type ( arguments )` (14.5.10.1).
    ObjectCreation {
        /// The type being created (a non-array type).
        target: TypeRef,
        /// The constructor arguments, in order.
        arguments: Vec<Expr>,
        /// The object or collection initializer `{ ... }` that follows (C# 3.0), if written.
        ///
        /// Independent of `arguments`: `new C(1) { F = 2 }` has both, `new C { F = 2 }` has only
        /// this, and `new C()` has neither. An EMPTY `new C { }` is `Some` of an empty object
        /// initializer rather than `None` -- the braces were written, and csc reports an empty one
        /// as an OBJECT initializer.
        initializer: Option<Initializer>,
    },
    /// An array creation `new element[lengths] extra-ranks` (14.5.10.2). When
    /// `lengths` is empty the size came from an initializer, which is not yet
    /// parsed; `rank` is the first dimension's rank and `extra_ranks` the trailing
    /// jagged ranks.
    ArrayCreation {
        /// The element (non-array) type.
        element: TypeRef,
        /// The size expressions of the first dimension; empty if unsized.
        lengths: Vec<Expr>,
        /// The rank of the first dimension.
        rank: u8,
        /// Trailing jagged rank-specifiers, outermost first.
        extra_ranks: Vec<u8>,
        /// The `{ ... }` initializer, if present.
        initializer: Option<Box<Expr>>,
    },
    /// An array initializer `{ e, ... }` (14.5.10.2). Grammatically valid only as a
    /// variable initializer or array-creation initializer; the binder enforces
    /// that. Elements may themselves be array initializers (nested).
    ArrayInitializer(Vec<Expr>),
    /// A placeholder for an expression that could not be parsed. It is emitted
    /// with a diagnostic so the parser can keep building a tree for the rest.
    Error,
}

/// Whether a [`ExprKind::TypeTest`] is an `is` or an `as` (14.9.9, 14.9.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTestOperation {
    /// `is`: tests whether the operand is of the type, yielding a `bool`.
    Is,
    /// `as`: converts to the type or yields `null`, never throwing.
    As,
}

/// A reference to a type (ECMA-334 1st ed, clause 11): a predefined type, a
/// (possibly qualified) type name, or an array of one of those. Pointer types
/// (unsafe code) are deferred with the rest of unsafe support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRef {
    /// What the type is, with any element type.
    pub kind: TypeRefKind,
    /// The byte range the type covers in the source.
    pub span: Span,
    /// Whether the type's FIRST name part was written as a verbatim identifier -- the `@var` of
    /// `@var x = 5;` (9.4.2).
    ///
    /// **THE NAME ITSELF CANNOT CARRY IT, WHICH IS WHY IT IS A SEPARATE FIELD.** The lexer
    /// canonicalizes `@var` to the name `var` -- correctly, since that is the identifier it
    /// denotes and it must bind to a type actually called `var` -- so by the time a `TypeRef`
    /// exists the two spellings are indistinguishable by text. What the prefix decides is
    /// whether the name may be read as a CONTEXTUAL KEYWORD at all, and the answer is no: a
    /// verbatim `@var` is an ordinary type name, so a program that declares no type called `var`
    /// gets CS0246 where the keyword would have inferred a type.
    ///
    /// Only the first part, and only a NAME: `N.@var` and `@var[]` are ordinary type references
    /// whose name happens to be spelled verbatim somewhere, and neither could have been the
    /// keyword under any reading.
    pub verbatim_name: bool,
}

impl TypeRef {
    /// Creates a type reference of `kind` covering `span`, whose name was not written verbatim.
    #[must_use]
    pub fn new(kind: TypeRefKind, span: Span) -> TypeRef {
        TypeRef {
            kind,
            span,
            verbatim_name: false,
        }
    }

    /// This type reference with [`TypeRef::verbatim_name`] set to `verbatim` -- how the parser
    /// records a `@`-prefixed first name part, which it can see and no later phase can.
    #[must_use]
    pub fn with_verbatim_name(mut self, verbatim: bool) -> TypeRef {
        self.verbatim_name = verbatim;
        self
    }
}

/// The kind of a [`TypeRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRefKind {
    /// A predefined type keyword, such as `int` or `string` (11.1.4).
    Predefined(PredefinedType),
    /// A type name, its parts in order: `A.B.C` is `["A", "B", "C"]` (11.1).
    Name(Vec<Box<str>>),
    /// A constructed generic type -- `A.B.C<T, U>`, and equally `A<T>.B` and `A<T>.B<U>`, where a
    /// type-argument list sits on an INTERIOR part (C# 2.0; ECMA-334 4th ed 10.8, 25.5).
    ///
    /// **EVERY PART CARRIES ITS OWN LIST, BECAUSE THAT IS WHAT THE GRAMMAR SAYS.** A
    /// `namespace-or-type-name` is `identifier type-argument-list_opt` repeated over the dots, so
    /// `List<int>.Enumerator` is as ordinary a name as `List<int>`. ONE list for a whole dotted
    /// name cannot tell `List<int>.Enumerator` from `List.Enumerator<int>`, and those are two
    /// different types -- so a single list is not a simplification of this, it is a conflation.
    ///
    /// **AT LEAST ONE PART HAS ARGUMENTS.** A name carrying no list anywhere is a
    /// [`TypeRefKind::Name`], and keeping the two apart is what lets `Box` and `Box<T>` be
    /// different types rather than one type with a sometimes-empty list -- the arity is part of the
    /// identity (25.5.1), which is also why the metadata spells it with a backtick.
    Generic {
        /// The name's parts in order, each with the type-argument list written on it.
        parts: Vec<TypeNamePart>,
    },
    /// An UNBOUND generic type -- `List<>`, `Dictionary<,>` -- the generic definition named with a
    /// `generic-dimension-specifier` in place of a type-argument list (ECMA-334 4th ed 14.5.11).
    ///
    /// **THE GRAMMAR ADMITS THIS IN ONE POSITION ONLY: THE OPERAND OF `typeof`.** `unbound-type-name`
    /// appears in no other production, and 25.5 says so in terms -- *an unbound generic type can
    /// only be used within a typeof-expression*. So the parser builds this from
    /// [`Parser::parse_typeof_operand`](crate::parser::Parser) and nowhere else, which is why every
    /// other type position still refuses `List<>` without having to say so itself.
    ///
    /// `arity` is the specifier's comma count PLUS ONE: `<>` is 1, `<,>` is 2. It is never 0 --
    /// a name carrying no specifier at all is a [`TypeRefKind::Name`], which is 14.5.11's own
    /// tie-break for a token sequence that satisfies both grammars.
    Unbound {
        /// The generic definition's name parts in order, as [`TypeRefKind::Name`] carries them.
        parts: Vec<Box<str>>,
        /// How many type parameters the definition takes; at least one.
        arity: usize,
    },
    /// A NULLABLE VALUE TYPE `T?` (C# 2.0; ECMA-334 4th ed 11.4): the underlying type carrying
    /// the `?` modifier.
    ///
    /// **`T?` AND `System.Nullable<T>` DENOTE THE SAME TYPE (11.4), AND `bind_type` MAPS THIS TO
    /// THAT INSTANTIATION** -- so no later phase needs a nullable case at all. The token table, the
    /// `TypeSpec`, member lookup for `HasValue`/`Value` and emission are the ones a constructed
    /// generic struct already has.
    ///
    /// **THE SPELLING SURVIVES PARSING BECAUSE A DIAGNOSTIC QUOTES IT.** csc says *Cannot
    /// implicitly convert type 'string' to 'int?'*, never `System.Nullable<int>`, so the `?` form
    /// has to reach the binder even though what it denotes is an ordinary constructed type.
    Nullable(Box<TypeRef>),
    /// An array type (12.1): an element type and the rank (dimension count) of
    /// this array. `int[][]` nests an `Array` whose element is another `Array`.
    Array {
        /// The element type.
        element: Box<TypeRef>,
        /// The number of dimensions, so `T[]` is 1 and `T[,]` is 2.
        rank: u8,
    },
    /// An unsafe pointer type (III.1.1.5): `T*`. `int**` nests a `Pointer` whose element is
    /// another `Pointer`. The pointed-to type is the element.
    Pointer(Box<TypeRef>),
    /// A BY-REFERENCE return type: the `ref` of `ref T M()`, `ref T this[int i]` and
    /// `ref T P { get; }` (C# 7.0), and `ref readonly T` (C# 7.2). The referent is the element.
    ///
    /// **[`Parser::parse_type`](crate::parser::Parser) NEVER PRODUCES ONE**, so a local, a type
    /// argument, an array element and a parameter's type cannot reach this variant however the
    /// source is spelled -- there is no arm to reach. It is built by `parse_member_type` alone.
    ///
    /// `Property` is what lets `bind_type` answer for all three in one arm, the way a `ref`
    /// PARAMETER is `TypeSymbol::ByRef` by the time anything downstream sees it.
    ///
    /// `is_readonly` is the `ref readonly T` form, whose metadata is `T&` plus a `modreq` on
    /// `System.Runtime.InteropServices.InAttribute` -- a different signature from a plain `ref T`,
    /// which is why the two are one variant with a flag rather than two spellings of one type.
    ByRef {
        /// The type being returned by reference.
        referent: Box<TypeRef>,
        /// Whether it was written `ref readonly`.
        is_readonly: bool,
    },
    /// A placeholder for a type that could not be parsed, emitted with a
    /// diagnostic for recovery.
    Error,
}

/// One part of a constructed type name: an identifier and the type-argument list written ON that
/// identifier (ECMA-334 4th ed 10.8, `namespace-or-type-name`).
///
/// **AN EMPTY `arguments` IS A STATEMENT, NOT AN ABSENCE.** It says this part introduces no type
/// parameters of its own -- which is exactly what ECMA-335 II.10.7.2 mangles into a metadata name,
/// so `List<int>.Enumerator` reads `` List`1 `` then `Enumerator` and the two representations line
/// up without a conversion. `System.Collections.Generic.List<int>` is four such parts, three of
/// them empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeNamePart {
    /// The identifier this part names.
    pub name: Box<str>,
    /// The type arguments written on this part, in order; empty when it carries no list.
    pub arguments: Vec<TypeRef>,
}

/// A predefined type (ECMA-334 1st ed, 11.1.4): the type keywords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredefinedType {
    /// `bool`.
    Bool,
    /// `byte`.
    Byte,
    /// `sbyte`.
    Sbyte,
    /// `short`.
    Short,
    /// `ushort`.
    Ushort,
    /// `int`.
    Int,
    /// `uint`.
    Uint,
    /// `long`.
    Long,
    /// `ulong`.
    Ulong,
    /// `char`.
    Char,
    /// `float`.
    Float,
    /// `double`.
    Double,
    /// `decimal`.
    Decimal,
    /// `string`.
    String,
    /// `object`.
    Object,
    /// `void`, valid only in a few positions but parsed uniformly here.
    Void,
}

impl PredefinedType {
    /// The C# keyword this type is written with, `int` for [`PredefinedType::Int`].
    #[must_use]
    pub const fn keyword(self) -> &'static str {
        match self {
            PredefinedType::Bool => "bool",
            PredefinedType::Byte => "byte",
            PredefinedType::Sbyte => "sbyte",
            PredefinedType::Short => "short",
            PredefinedType::Ushort => "ushort",
            PredefinedType::Int => "int",
            PredefinedType::Uint => "uint",
            PredefinedType::Long => "long",
            PredefinedType::Ulong => "ulong",
            PredefinedType::Char => "char",
            PredefinedType::Float => "float",
            PredefinedType::Double => "double",
            PredefinedType::Decimal => "decimal",
            PredefinedType::String => "string",
            PredefinedType::Object => "object",
            PredefinedType::Void => "void",
        }
    }
}

/// A statement: a [`StmtKind`] and the source [`Span`] it covers (clause 15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stmt {
    /// What kind of statement this is, with its children.
    pub kind: StmtKind,
    /// The byte range the statement covers in the source.
    pub span: Span,
}

impl Stmt {
    /// Creates a statement of `kind` covering `span`.
    #[must_use]
    pub fn new(kind: StmtKind, span: Span) -> Stmt {
        Stmt { kind, span }
    }
}

/// The kind of a [`Stmt`] (ECMA-334 1st ed, clause 15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StmtKind {
    /// A block `{ ... }` (15.2).
    Block(Vec<Stmt>),
    /// The empty statement `;` (15.3).
    Empty,
    /// An expression statement `expression ;` (15.6). Binding checks that the
    /// expression is one allowed as a statement (a call, assignment, increment,
    /// decrement, or object creation).
    Expression(Expr),
    /// A local variable declaration `type declarators ;` (15.5.1), or a local
    /// constant declaration `const type declarators ;` when `is_const` (15.5.1
    /// declares a local constant whose value is a constant expression, 14.15).
    LocalDeclaration {
        /// The declared type, shared by every declarator.
        ty: TypeRef,
        /// The declared variables, in order.
        declarators: Vec<VariableDeclarator>,
        /// Whether a leading `const` modifier made this a local constant declaration:
        /// each declarator's initializer is a constant expression folded at compile
        /// time, and the name has no storage.
        is_const: bool,
    },
    /// A `return` statement, with its optional value (15.9.4).
    Return(Option<Expr>),
    /// An `if` statement with an optional `else` branch (15.7.1).
    If {
        /// The condition tested.
        condition: Expr,
        /// The statement run when the condition is true.
        then_branch: Box<Stmt>,
        /// The statement run otherwise, if an `else` is present.
        else_branch: Option<Box<Stmt>>,
    },
    /// A `while` statement (15.8.1).
    While {
        /// The loop condition.
        condition: Expr,
        /// The loop body.
        body: Box<Stmt>,
    },
    /// A `do body while ( condition ) ;` statement (15.8.2).
    DoWhile {
        /// The loop body, run before the first test.
        body: Box<Stmt>,
        /// The condition tested after each iteration.
        condition: Expr,
    },
    /// A `for` statement (15.8.3).
    For {
        /// The initializer clause, if any.
        initializer: Option<ForInitializer>,
        /// The loop condition, if any.
        condition: Option<Expr>,
        /// The iterator expressions run after each iteration.
        iterators: Vec<Expr>,
        /// The loop body.
        body: Box<Stmt>,
    },
    /// A `foreach ( type name in collection ) body` statement (15.8.4).
    ForEach {
        /// The iteration variable's type.
        ty: TypeRef,
        /// The iteration variable's name.
        name: Box<str>,
        /// The collection iterated over.
        collection: Expr,
        /// The loop body.
        body: Box<Stmt>,
    },
    /// A `break ;` statement (15.9.1).
    Break,
    /// A `continue ;` statement (15.9.2).
    Continue,
    /// A `throw expression_opt ;` statement (15.9.5).
    Throw(Option<Expr>),
    /// A `try` statement with catch clauses and/or a finally block (15.10).
    Try {
        /// The protected block.
        body: Box<Stmt>,
        /// The catch clauses, in order.
        catches: Vec<CatchClause>,
        /// The finally block, if present.
        finally_block: Option<Box<Stmt>>,
    },
    /// A `lock ( expression ) statement` (15.12).
    Lock {
        /// The object locked on.
        expression: Expr,
        /// The guarded statement.
        body: Box<Stmt>,
    },
    /// A `using ( resource ) statement` (15.13).
    Using {
        /// The resource acquired for the duration of the body.
        resource: UsingResource,
        /// The guarded statement.
        body: Box<Stmt>,
    },
    /// A `fixed ( T* id = expr ) statement` (unsafe, 15.7): pins `expr` (an array/string)
    /// for the body and binds `id` to a pointer to its first element.
    Fixed {
        /// The pointer-variable type (`T*`).
        ty: TypeRef,
        /// The pointer variable bound for the body.
        name: Box<str>,
        /// The pinned source (an array or string).
        init: Expr,
        /// The guarded statement.
        body: Box<Stmt>,
    },
    /// A `checked` block statement (15.11), forcing overflow checking on.
    Checked(Box<Stmt>),
    /// An `unchecked` block statement (15.11), forcing overflow checking off.
    Unchecked(Box<Stmt>),
    /// A `switch` statement (15.7.2).
    Switch {
        /// The value switched on.
        expression: Expr,
        /// The switch sections, in order.
        sections: Vec<SwitchSection>,
    },
    /// A labeled statement `label : statement` (15.4).
    Labeled {
        /// The label name.
        label: Box<str>,
        /// The labeled statement.
        statement: Box<Stmt>,
    },
    /// A `goto` statement (15.9.3).
    Goto(GotoTarget),
    /// A placeholder for a statement that could not be parsed, emitted with a
    /// diagnostic for recovery.
    Error,
}

/// One section of a `switch` statement (15.7.2): its labels and statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchSection {
    /// The `case`/`default` labels introducing the section.
    pub labels: Vec<SwitchLabel>,
    /// The statements run when a label matches.
    pub statements: Vec<Stmt>,
}

/// A `switch` label (15.7.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchLabel {
    /// `case constant-expression :`.
    Case(Expr),
    /// `default :`.
    Default,
}

/// The target of a `goto` statement (15.9.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GotoTarget {
    /// `goto label ;`.
    Label(Box<str>),
    /// `goto case constant-expression ;`.
    Case(Expr),
    /// `goto default ;`.
    Default,
}

/// An attribute section `[ target? attribute-list ]` (clause 24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeSection {
    /// The target specifier before `:` (for example `assembly`), if any.
    pub target: Option<Box<str>>,
    /// The attributes in the section.
    pub attributes: Vec<Attribute>,
    /// The byte range the section covers.
    pub span: Span,
}

/// One attribute within an [`AttributeSection`] (24.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    /// The attribute (type) name.
    pub name: QualifiedName,
    /// The positional and named arguments, in order.
    pub arguments: Vec<AttributeArgument>,
    /// The byte range the attribute covers.
    pub span: Span,
}

/// An argument to an [`Attribute`] (24.2): positional or named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeArgument {
    /// A positional argument expression.
    Positional(Expr),
    /// A named argument `name = expression`.
    Named {
        /// The parameter or field/property name.
        name: Box<str>,
        /// The argument value.
        value: Expr,
    },
}

/// A whole source file (ECMA-334 1st ed, 16.1): using directives then the
/// top-level namespace and type declarations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilationUnit {
    /// The file-level using directives.
    pub usings: Vec<UsingDirective>,
    /// The top-level namespace and type declarations.
    pub members: Vec<NamespaceMember>,
    /// The assembly-/module-level global attributes (24.2) -- `[assembly: ...]` / `[module: ...]`
    /// sections (typically in an AssemblyInfo.cs). They attach to the assembly/module manifest, not
    /// to any declaration.
    pub global_attributes: Vec<AttributeSection>,
    /// The byte range the unit covers.
    pub span: Span,
    /// The `#define`d preprocessor symbols (9.5.3) -- the set a `[Conditional]` call is checked
    /// against to decide inclusion (24.4.2). Empty when none are defined.
    pub defined_symbols: BTreeSet<Box<str>>,
}

/// A dotted name such as `System.Collections` (10.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifiedName {
    /// The dot-separated parts, in order.
    pub parts: Vec<Box<str>>,
    /// The byte range the name covers.
    pub span: Span,
}

/// An object or collection initializer following a `new` (C# 3.0).
///
/// **Which one it is comes from the FIRST element, and csc names them differently in its
/// diagnostics** (`'object initializer'` vs `'collection initializer'`), so they are separate
/// variants rather than one list with a flag. An empty `{ }` is [`Initializer::Object`] --
/// measured, and it is the tie-break the parser needs for a case with nothing to look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Initializer {
    /// `{ F = 1, P = 2 }` -- assignments into the new object's own members.
    Object(Vec<MemberInitializer>),
    /// `{ 1, 2 }` -- elements handed to the type's `Add` method.
    ///
    /// The type must implement `IEnumerable` (csc CS1922) and have an applicable `Add`
    /// (CS1061); neither is a syntactic condition, so both are the binder's to enforce.
    Collection(Vec<Expr>),
}

/// One `name = value` inside an object initializer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberInitializer {
    /// The member being initialized. A plain name, never qualified.
    pub name: Box<str>,
    /// What is assigned to it.
    pub value: MemberInitializerValue,
    /// The byte range the whole `name = value` covers.
    pub span: Span,
}

/// The right-hand side of a member initializer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberInitializerValue {
    /// `F = expr` -- an ordinary assignment.
    Expression(Expr),
    /// `F = { ... }` -- a NESTED initializer.
    ///
    /// **This does NOT construct anything.** `new C { F = { G = 1 } }` assigns into the object
    /// `F` ALREADY refers to, so a null `F` is a run-time failure rather than a fresh `D`. Reading
    /// it as an implicit `new` is the natural mistake and it is the opposite of what it does.
    Nested(Initializer),
}

/// A `using` directive (16.3): import a namespace, import a type's statics, or define an alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsingDirective {
    /// What the directive imports.
    pub kind: UsingKind,
    /// The byte range it covers.
    pub span: Span,
}

/// The kind of a [`UsingDirective`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsingKind {
    /// `using A.B.C ;` -- bring a namespace's members into scope.
    Namespace(QualifiedName),
    /// `using static A.B ;` (C# 6.0) -- bring one TYPE's directly declared static members and
    /// nested types into scope, nameable without qualification (ECMA-334 6th ed, 13.5.4).
    ///
    /// **The operand is a `type_name`, not a namespace**, which is the whole difference from
    /// [`UsingKind::Namespace`] and the reason it is a separate variant rather than a flag: csc
    /// reports `CS7007` for `using static System;`, naming the mistake, where a namespace import
    /// of the same text is correct.
    Static(QualifiedName),
    /// `using X = A.B.C ;` -- an alias for a namespace or type.
    Alias {
        /// The alias identifier.
        name: Box<str>,
        /// The aliased namespace or type name.
        target: QualifiedName,
    },
}

/// A member of a compilation unit or namespace (16.4). Also used for a type
/// nested in another type (17.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceMember {
    /// A nested namespace.
    Namespace(NamespaceDecl),
    /// A class, struct, or interface declaration.
    Type(TypeDecl),
    /// An enum declaration.
    Enum(EnumDecl),
    /// A delegate declaration.
    Delegate(DelegateDecl),
}

/// An `enum` declaration (21.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumDecl {
    /// The attribute sections applied to the enum.
    pub attributes: Vec<AttributeSection>,
    /// The declared modifiers, in source order.
    pub modifiers: Vec<Modifier>,
    /// The enum's name.
    pub name: Box<str>,
    /// The underlying integral type, if given after `:`.
    pub base: Option<TypeRef>,
    /// The enum members, in order.
    pub members: Vec<EnumMember>,
    /// The byte range the declaration covers.
    pub span: Span,
}

/// One member of an [`EnumDecl`] (21.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumMember {
    /// The attribute sections applied to the member (24.2). An enum member is an attribute
    /// TARGET like any other declaration, so `[Obsolete] Value` parses.
    pub attributes: Vec<AttributeSection>,
    /// The member's name.
    pub name: Box<str>,
    /// The constant value expression, if given with `=`.
    pub value: Option<Expr>,
    /// The byte range the member covers.
    pub span: Span,
}

/// A `delegate` declaration (22.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateDecl {
    /// The attribute sections applied to the delegate.
    pub attributes: Vec<AttributeSection>,
    /// The declared modifiers, in source order.
    pub modifiers: Vec<Modifier>,
    /// The delegate's return type.
    pub return_type: TypeRef,
    /// The delegate's name.
    pub name: Box<str>,
    /// The delegate's formal parameters.
    pub parameters: Vec<Parameter>,
    /// The byte range the declaration covers.
    pub span: Span,
}


/// A `namespace` declaration (16.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceDecl {
    /// The (possibly dotted) namespace name.
    pub name: QualifiedName,
    /// The namespace body's using directives.
    pub usings: Vec<UsingDirective>,
    /// The namespace body's member declarations.
    pub members: Vec<NamespaceMember>,
    /// Whether the declaration was written file-scoped -- `namespace N;` (C# 10) rather than
    /// `namespace N { ... }`.
    ///
    /// **The two forms declare the same namespace and differ in nothing a consumer of this tree
    /// can act on**, which is why the members are the same field: a file-scoped declaration's body
    /// is everything up to the end of its container, already collected here. The flag records what
    /// the source said, for a tree dump and for anything that has to reproduce the source shape;
    /// the binder and the emitter ignore it.
    pub file_scoped: bool,
    /// The byte range the declaration covers.
    pub span: Span,
}

/// A class, struct, or interface declaration (17, 18, 20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDecl {
    /// The attribute sections applied to the type.
    pub attributes: Vec<AttributeSection>,
    /// The declared modifiers, in source order.
    pub modifiers: Vec<Modifier>,
    /// Whether this is a class, struct, or interface.
    pub kind: TypeKind,
    /// The type's name.
    pub name: Box<str>,
    /// The type parameters declared after the name (C# 2.0): `class Box<T>` holds one. Empty for
    /// an ordinary type, which is what every C# 1.0 declaration is.
    pub type_parameters: Vec<TypeParameter>,
    /// The base class and/or interfaces listed after `:`.
    pub bases: Vec<TypeRef>,
    /// The `where` clauses written after the base list (25.7). Empty for an ordinary type, and
    /// empty for a generic type that constrains nothing.
    pub constraints: Vec<TypeParameterConstraintClause>,
    /// The type's members.
    pub members: Vec<Member>,
    /// The RECORD half of the declaration (C# 9), or `None` for an ordinary class, struct or
    /// interface.
    ///
    /// **A RECORD IS A CLASS AND IS MODELLED AS ONE**, with [`TypeDecl::kind`] still
    /// [`TypeKind::Class`]. A fourth `TypeKind` variant would make every existing
    /// `TypeKind::Class` arm wrong by omission -- a record IS-A class at nearly all of them, so
    /// the default behaviour has to be the class behaviour and only the sites that synthesize
    /// members may ask. `record struct` (C# 10) is the same field on a `TypeKind::Struct`.
    pub record: Option<RecordParts>,
    /// The byte range the declaration covers.
    pub span: Span,
}

/// What a `record` declaration carries beyond an ordinary class (C# 9).
///
/// Its presence on a [`TypeDecl`] is what makes the type a record; the fields describe only the
/// parts that vary between record FORMS, because everything else csc generates -- value equality,
/// `ToString`/`PrintMembers`, `<Clone>$`, the copy constructor -- is emitted for every form alike
/// and so needs nothing recorded here. Measured over three forms against csc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordParts {
    /// The positional parameter list, `None` when none was written.
    ///
    /// **`None` AND `Some(vec![])` ARE DIFFERENT DECLARATIONS**: `record R { }` has no parameter
    /// list and gets a parameterless constructor and no `Deconstruct`; `record R();` HAS an empty
    /// one and gets both. Collapsing them to a `Vec` would make the two forms identical here and
    /// silently drop a `Deconstruct` csc emits.
    pub parameters: Option<Vec<Parameter>>,
    /// The argument list on a base record -- the `(X)` of `record D(int X, int Y) : B(X)` (14.5.11
    /// applies to the arguments; the base type itself is the first entry in [`TypeDecl::bases`]).
    /// `None` when the base list carries no argument list.
    pub base_arguments: Option<Vec<Expr>>,
    /// Whether the `class` or `struct` keyword was written after `record` -- `record class R` and
    /// `record struct R`, which are a SEPARATE csc feature at C# 10 called `'record structs'`,
    /// PLURAL, for the class form too. Measured one compilation per rung.
    pub keyword_form: bool,
    /// The byte range the `record` keyword itself covers, which is where the feature gate points.
    pub keyword_span: Span,
}

/// One declared type parameter (C# 2.0): the `T` in `class Box<T>` or `T M<T>(T)`.
///
/// The name is the whole of it. Constraints (`where T : IComparable`) are a separate clause that
/// follows the parameter LIST rather than the parameter, so they are not a field here; they live
/// on the declaration as [`TypeParameterConstraintClause`], which is the shape the grammar has
/// (25.7) and the shape that lets a clause name a parameter that was never declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParameter {
    /// The parameter's name.
    pub name: Box<str>,
    /// The byte range the name covers.
    pub span: Span,
}

/// One `where` clause (C# 2.0; ECMA-334 4th ed 25.7): `where T : class, IComparable, new()`.
///
/// **A clause names its parameter rather than being positional**, which is why this is a list on
/// the declaration and not a field on [`TypeParameter`]: the grammar permits clauses in any order,
/// permits a parameter to have none, and permits a clause to name an identifier that is not a type
/// parameter at all (CS0699). Binding it positionally would make that last case unrepresentable and
/// so unreportable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParameterConstraintClause {
    /// The type parameter the clause constrains -- the `T` in `where T : class`.
    pub parameter: Box<str>,
    /// The byte range the parameter name covers, which is where CS0699 points.
    pub parameter_span: Span,
    /// The constraints, in the order written. The order is not free in the language (a `class` or
    /// `struct` constraint must come first and `new()` last), so preserving it is what lets the
    /// binder report CS0401/CS0449 rather than silently accepting a reordering.
    pub constraints: Vec<TypeParameterConstraint>,
    /// The byte range the whole clause covers, from `where` through the last constraint.
    pub span: Span,
}

/// One constraint inside a [`TypeParameterConstraintClause`] (25.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeParameterConstraint {
    /// `class` -- the reference-type constraint. Metadata flag `0x0004`.
    ReferenceType(Span),
    /// `struct` -- the non-nullable value-type constraint. Metadata flag `0x0008`, and it implies
    /// `0x0010`: every value type has a parameterless constructor, so `struct` subsumes `new()`,
    /// which is why writing both is CS0451 rather than a redundancy.
    ValueType(Span),
    /// `new()` -- the constructor constraint. Metadata flag `0x0010`.
    DefaultConstructor(Span),
    /// A named class, interface, or type parameter constraint. Unlike the three above this is a
    /// real type reference and becomes a `GenericParamConstraint` row rather than a flag bit.
    Type(TypeRef),
}

impl TypeParameterConstraint {
    /// The byte range the constraint covers.
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            TypeParameterConstraint::ReferenceType(span)
            | TypeParameterConstraint::ValueType(span)
            | TypeParameterConstraint::DefaultConstructor(span) => *span,
            TypeParameterConstraint::Type(reference) => reference.span,
        }
    }
}

/// Which kind of type a [`TypeDecl`] declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    /// `class`.
    Class,
    /// `struct`.
    Struct,
    /// `interface`.
    Interface,
}

/// A declaration modifier (the parser accepts any; binding checks validity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    /// `new`.
    New,
    /// `public`.
    Public,
    /// `protected`.
    Protected,
    /// `internal`.
    Internal,
    /// `private`.
    Private,
    /// `abstract`.
    Abstract,
    /// `sealed`.
    Sealed,
    /// `static`.
    Static,
    /// `readonly`.
    Readonly,
    /// `volatile`.
    Volatile,
    /// `virtual`.
    Virtual,
    /// `override`.
    Override,
    /// `extern`.
    Extern,
    /// `const`.
    Const,
    /// `unsafe`.
    Unsafe,
    /// `partial` (C# 2.0, ECMA-334 4th ed 17.1.4) -- this declaration is one PART of a type whose
    /// other parts are declared elsewhere, possibly in another file.
    ///
    /// CONTEXTUAL, like [`Modifier::Required`] and [`Modifier::Async`], and more tightly placed
    /// than either: 17.1.4 admits it only IMMEDIATELY before `class`, `struct` or `interface`, so
    /// `class partial { }` and a field of type `partial` both keep compiling (measured against
    /// csc). See `Parser::partial_is_a_modifier_here`.
    Partial,
    /// `required` (C# 11) -- an initializer the caller MUST supply.
    ///
    /// One of the two CONTEXTUAL modifiers in this list (see [`Modifier::Async`]), so unlike its
    /// neighbours it is not produced by mapping a [`crate::token::Keyword`]: `required` is an
    /// ordinary identifier everywhere else, and a field, local, parameter or type may still be
    /// named it. See `Parser::required_is_a_modifier_here` for the two-token lookahead that tells
    /// the two apart.
    Required,
    /// `async` (C# 5, ECMA-334 5th ed 15.15) -- the method is an async function and `await` is
    /// reserved in its body.
    ///
    /// CONTEXTUAL, like [`Modifier::Required`] and unlike everything else here: `async` stays an
    /// ordinary identifier elsewhere, so `class async { }`, a field of type `async`, and a method
    /// `async async()` RETURNING that type all keep compiling (measured against csc). See
    /// `Parser::async_is_a_modifier_here` for the lookahead -- which, unlike `required`'s
    /// two-token peek, must scan a full type, because `async Task<int> M()` puts arbitrarily many
    /// tokens between the modifier and the name that proves it is one.
    Async,
    /// `ref` on a STRUCT declaration (C# 7.2): `ref struct S { }`, and `readonly ref struct`
    /// with `readonly` first -- `ref readonly struct` is CS1031 under csc, measured.
    ///
    /// **A `ref struct` IS A STRUCT AND IS MODELLED AS ONE**, for the reason
    /// [`TypeDecl::record`] gives: a fourth [`TypeKind`] would make every existing
    /// [`TypeKind::Struct`] arm wrong by omission, when a ref struct IS-A struct at nearly all
    /// of them and only the sites that enforce the by-ref-like restrictions may ask.
    ///
    /// POSITIONAL, not contextual: `ref` is a keyword everywhere, but it is a MODIFIER only
    /// immediately before `struct` or before `partial struct`. `ref class` and `ref interface`
    /// are CS1031 under csc -- it does not take `ref` as a modifier there either -- and a
    /// `ref`-returning member (`ref int P => ...`) must keep parsing as one. See
    /// `Parser::ref_is_a_modifier_here`.
    Ref,
}

/// A member of a type (17.2). Fields, methods, and constructors land first;
/// properties, indexers, events, operators, constants, and nested types follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Member {
    /// A field declaration `modifiers type declarators ;` (17.4).
    Field {
        /// The member's modifiers.
        modifiers: Vec<Modifier>,
        /// The field type.
        ty: TypeRef,
        /// The declared fields.
        declarators: Vec<VariableDeclarator>,
        /// The member's attributes (24.2).
        attributes: Vec<AttributeSection>,
        /// The byte range the member covers.
        span: Span,
    },
    /// A method declaration (17.5). The body is `None` for an abstract, extern,
    /// or interface method (a `;` in place of a block).
    Method {
        /// The member's modifiers.
        modifiers: Vec<Modifier>,
        /// The return type.
        return_type: TypeRef,
        /// The method name.
        name: Box<str>,
        /// The type parameters declared after the name (C# 2.0): the `T` in `T M<T>(T x)`. Empty
        /// for an ordinary method. These are the method's OWN parameters, distinct from any the
        /// enclosing type declares -- the distinction the metadata encoding spells `!!0` against
        /// `!0`.
        type_parameters: Vec<TypeParameter>,
        /// The `where` clauses written after the parameter list (25.7). Empty for an ordinary
        /// method.
        constraints: Vec<TypeParameterConstraintClause>,
        /// The formal parameters.
        parameters: Vec<Parameter>,
        /// Whether the parameter list ends with csc's `__arglist` marker (parsed only under
        /// the typedref knob): the method takes variable arguments via the CLI vararg calling
        /// convention, beyond `parameters`.
        is_vararg: bool,
        /// The method body, or `None` for a bare `;`.
        body: Option<Stmt>,
        /// For an explicit interface member implementation (20.4.1), the interface
        /// the method implements -- the part before the final dot of a qualified
        /// name like `I.M`. `None` for an ordinary method. Such a method is callable
        /// only through the interface, never by its simple name.
        explicit_interface: Option<TypeRef>,
        /// The member's attributes (24.2).
        attributes: Vec<AttributeSection>,
        /// The byte range the member covers.
        span: Span,
    },
    /// An instance or static constructor (17.10, 17.11): a name matching the
    /// type, no return type, an optional `: base(...)`/`: this(...)` initializer,
    /// then a body.
    Constructor {
        /// The member's modifiers.
        modifiers: Vec<Modifier>,
        /// The constructor name (the type name).
        name: Box<str>,
        /// The formal parameters.
        parameters: Vec<Parameter>,
        /// Whether the parameter list ends with csc's `__arglist` marker (typedref knob):
        /// the constructor takes variable arguments via the CLI vararg calling convention.
        is_vararg: bool,
        /// The `: base(...)` or `: this(...)` initializer, if present.
        initializer: Option<ConstructorInitializer>,
        /// The constructor body.
        body: Stmt,
        /// The signature header -- the declaration start through the parameter list's
        /// close paren -- for the debug sequence point on an implicit `: base()` call
        /// (csc stops there before the body, so `step into` a constructor halts on it).
        header_span: Span,
        /// The member's attributes (24.2).
        attributes: Vec<AttributeSection>,
        /// The byte range the member covers.
        span: Span,
    },
    /// A property declaration (17.6): `modifiers type name { accessors }`.
    Property {
        /// The member's modifiers.
        modifiers: Vec<Modifier>,
        /// The property type.
        ty: TypeRef,
        /// The property name.
        name: Box<str>,
        /// The `get` accessor, if present.
        getter: Option<Accessor>,
        /// The `set` accessor, if present.
        setter: Option<Accessor>,
        /// The explicitly implemented interface for `int I.P { ... }` (20.4.1), naming
        /// its accessors `I.get_P`/`I.set_P`. `None` for an ordinary property.
        explicit_interface: Option<TypeRef>,
        /// The AUTO-PROPERTY INITIALIZER's expression, `int P { get; set; } = 5;` (C# 6.0), else
        /// `None`.
        ///
        /// It initializes the BACKING FIELD and not the property, which is what lets a getter-only
        /// auto-property carry one -- there is no setter to call -- and what keeps a virtual setter
        /// from running before the derived constructor has. So it is stored beside the accessors
        /// rather than inside one, and lowers exactly where a field initializer does.
        initializer: Option<Expr>,
        /// The member's attributes (24.2).
        attributes: Vec<AttributeSection>,
        /// The byte range the member covers.
        span: Span,
    },
    /// A field-like event declaration (17.7): `modifiers event type declarators ;`.
    EventField {
        /// The member's modifiers.
        modifiers: Vec<Modifier>,
        /// The event (delegate) type.
        ty: TypeRef,
        /// The declared events.
        declarators: Vec<VariableDeclarator>,
        /// The member's attributes (24.2).
        attributes: Vec<AttributeSection>,
        /// The byte range the member covers.
        span: Span,
    },
    /// An event declaration with accessors (17.7): `modifiers event type name
    /// { add ... remove ... }`.
    Event {
        /// The member's modifiers.
        modifiers: Vec<Modifier>,
        /// The event (delegate) type.
        ty: TypeRef,
        /// The event name.
        name: Box<str>,
        /// The `add` accessor, if present.
        adder: Option<Accessor>,
        /// The `remove` accessor, if present.
        remover: Option<Accessor>,
        /// The explicitly implemented interface for `event H I.E { ... }` (20.4.1), naming
        /// its accessors `I.add_E`/`I.remove_E`. `None` for an ordinary custom-accessor event.
        explicit_interface: Option<TypeRef>,
        /// The member's attributes (24.2).
        attributes: Vec<AttributeSection>,
        /// The byte range the member covers.
        span: Span,
    },
    /// An indexer declaration (17.8): `modifiers type this [ params ] { accessors }`.
    Indexer {
        /// The member's modifiers.
        modifiers: Vec<Modifier>,
        /// The element type.
        ty: TypeRef,
        /// The index formal parameters (at least one).
        parameters: Vec<Parameter>,
        /// The `get` accessor, if present.
        getter: Option<Accessor>,
        /// The `set` accessor, if present.
        setter: Option<Accessor>,
        /// The member's attribute sections (e.g. `[IndexerName("Chars")]`, which renames the
        /// accessors to `get_Chars`/`set_Chars`).
        attributes: Vec<AttributeSection>,
        /// The byte range the member covers.
        span: Span,
    },
    /// An overloaded unary or binary operator (17.9.1, 17.9.2): `modifiers
    /// return-type operator <op> ( params ) body`.
    Operator {
        /// The member's modifiers.
        modifiers: Vec<Modifier>,
        /// The operator's return type.
        return_type: TypeRef,
        /// The operator being defined.
        operator: OverloadableOperator,
        /// The operand parameters (one for unary, two for binary).
        parameters: Vec<Parameter>,
        /// The operator body.
        body: Stmt,
        /// The member's attributes (24.2), carried to the `op_*` method row.
        attributes: Vec<AttributeSection>,
        /// The byte range the member covers.
        span: Span,
    },
    /// A user-defined conversion operator (17.9.3): `modifiers implicit|explicit
    /// operator target ( param ) body`.
    ConversionOperator {
        /// The member's modifiers.
        modifiers: Vec<Modifier>,
        /// Whether the conversion is implicit or explicit.
        direction: ConversionDirection,
        /// The type converted to.
        target: TypeRef,
        /// The single source parameter.
        parameters: Vec<Parameter>,
        /// The operator body.
        body: Stmt,
        /// The member's attributes (24.2), carried to the `op_Implicit`/`op_Explicit` row.
        attributes: Vec<AttributeSection>,
        /// The byte range the member covers.
        span: Span,
    },
    /// A destructor (17.12): `~ name ( ) body`.
    Destructor {
        /// The member's modifiers (only `extern` is valid; the parser accepts any).
        modifiers: Vec<Modifier>,
        /// The destructor name (the type name).
        name: Box<str>,
        /// The destructor body.
        body: Stmt,
        /// The member's attributes (24.2), carried to the synthesized `Finalize` row.
        attributes: Vec<AttributeSection>,
        /// The byte range the member covers.
        span: Span,
    },
    /// A type nested in another type (17.2): a class, struct, interface, enum, or
    /// delegate. Boxed because [`NamespaceMember`] holds members in turn.
    NestedType(Box<NamespaceMember>),
    /// A placeholder for a member that could not be parsed, for recovery.
    Error,
}

impl Member {
    /// Attaches the parsed attribute sections to a member that carries them (24.2). A member
    /// kind that does not yet model attributes ignores them.
    pub fn set_attributes(&mut self, attributes: Vec<AttributeSection>) {
        match self {
            Member::Field { attributes: slot, .. }
            | Member::Method { attributes: slot, .. }
            | Member::Constructor { attributes: slot, .. }
            | Member::Property { attributes: slot, .. }
            | Member::Indexer { attributes: slot, .. }
            | Member::EventField { attributes: slot, .. }
            | Member::Event { attributes: slot, .. }
            | Member::Operator { attributes: slot, .. }
            | Member::ConversionOperator { attributes: slot, .. }
            | Member::Destructor { attributes: slot, .. } => *slot = attributes,
            _ => {}
        }
    }
}

/// Whether a property declaration is an AUTOMATICALLY IMPLEMENTED PROPERTY -- one the compiler
/// backs with a synthesized field and synthesized accessors, rather than one the program wrote a
/// body for.
///
/// **THREE ANSWERS DEPEND ON THIS AND THEY MUST BE THE SAME ANSWER**: the binder registers the
/// backing field in its model, the emitter's token pre-pass reserves the `FieldDef` row, and
/// emission writes it. A `TypeDef`'s field range runs to where the next type's begins (II.22.37),
/// so two of them disagreeing does not lose a field -- it shifts every field token after it. That
/// is why this lives in the syntax crate, which both of the others already depend on, rather than
/// being asked twice.
///
/// **THE GETTER IS REQUIRED AND THE SETTER IS NOT.** `{ get; }` is C# 6.0's readonly auto-property,
/// whose backing field is `initonly`; `{ get; set; }` is C# 3.0's. `{ set; }` is neither -- csc
/// answers CS8051, and a property with nothing to read it back through has no field to synthesize.
/// A half-written `{ get; set { ... } }` is not one either: an accessor with a body is the
/// program's implementation, and generating a field beside it would give the property two.
///
/// `is_interface` is the DECLARING type's kind, which the member alone cannot say. An interface
/// property, like an `abstract` or `extern` one, is bodyless because it declares only a contract.
#[must_use]
pub fn is_auto_property(
    modifiers: &[Modifier],
    getter: Option<&Accessor>,
    setter: Option<&Accessor>,
    is_interface: bool,
) -> bool {
    if is_interface
        || modifiers
            .iter()
            .any(|modifier| matches!(modifier, Modifier::Abstract | Modifier::Extern))
    {
        return false;
    }
    let Some(getter) = getter else {
        return false;
    };
    getter.body.is_none() && setter.is_none_or(|setter| setter.body.is_none())
}

/// The metadata name of an auto-property's backing field: csc's `<Name>k__BackingField`, and
/// `<IHas.N>k__BackingField` for an explicit interface implementation -- measured.
///
/// **NOT A LEGAL C# IDENTIFIER, ON PURPOSE.** The angle brackets are what keep it from colliding
/// with anything the program can declare, and copying csc's spelling exactly is what lets a
/// debugger, a serializer or a reflection-based tool recognize the field for what it is. It is also
/// what lets the binder hold the field under a name no source expression can reach, so an
/// initializer can be lowered onto it without the name becoming part of the language.
///
/// The interface qualifier is not decoration either: a type may explicitly implement `N` from two
/// different interfaces AND declare its own `N`, which is three distinct auto-properties whose
/// backing fields would otherwise share one name.
#[must_use]
pub fn auto_property_backing_field_name(
    explicit_interface: Option<&TypeRef>,
    property: &str,
) -> String {
    match explicit_interface {
        Some(interface) => alloc::format!(
            "<{}>k__BackingField",
            explicit_interface_member_name(interface, property)
        ),
        None => alloc::format!("<{property}>k__BackingField"),
    }
}

/// The name an explicit interface member implementation carries in metadata and in
/// the symbol model: the interface's source spelling, a dot, then the member -- e.g.
/// `I.M` or `System.IComparable.CompareTo`. csc names the `MethodDef` this way, and
/// registering it under this mangled name keeps ordinary simple-name lookup of the
/// member from finding it (so it is reachable only through the interface). `member`
/// is the bare member name; `interface_ref` is the qualifying interface type.
///
/// A CONSTRUCTED interface carries its type arguments, `IBox<int>.M`, and carrying them is
/// what keeps the two members of `class C : IBox<int>, IBox<string>` apart -- both are legal
/// on one class, and the ARITY they share cannot separate them.
///
/// The interface is spelled as it was WRITTEN, so two source spellings of one type key
/// differently. Only a declaration and the check that credits it are compared, and both reach
/// this function from the same syntax, so they agree by construction.
#[must_use]
pub fn explicit_interface_member_name(interface_ref: &TypeRef, member: &str) -> String {
    let mut name = String::new();
    match &interface_ref.kind {
        TypeRefKind::Name(parts) => {
            for part in parts {
                name.push_str(part);
                name.push('.');
            }
        }
        TypeRefKind::Generic { parts } => {
            for part in parts {
                name.push_str(&part.name);
                write_type_arguments(&mut name, &part.arguments);
                name.push('.');
            }
        }
        _ => {}
    }
    name.push_str(member);
    name
}

/// Appends `<a,b>` to `text` for a part's type arguments, or nothing when it carries none.
///
/// Shared by [`explicit_interface_member_name`] and [`write_type_ref`], so a part's arguments are
/// spelled one way wherever they appear.
fn write_type_arguments(text: &mut String, arguments: &[TypeRef]) {
    if arguments.is_empty() {
        return;
    }
    text.push('<');
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        write_type_ref(text, argument);
    }
    text.push('>');
}

/// Appends a type reference's source spelling to `text`, for the mangled names in
/// [`explicit_interface_member_name`].
fn write_type_ref(text: &mut String, ty: &TypeRef) {
    match &ty.kind {
        TypeRefKind::Predefined(predefined) => text.push_str(predefined.keyword()),
        TypeRefKind::Name(parts) => {
            for (index, part) in parts.iter().enumerate() {
                if index > 0 {
                    text.push('.');
                }
                text.push_str(part);
            }
        }
        TypeRefKind::Generic { parts } => {
            for (index, part) in parts.iter().enumerate() {
                if index > 0 {
                    text.push('.');
                }
                text.push_str(&part.name);
                write_type_arguments(text, &part.arguments);
            }
        }
        TypeRefKind::Nullable(underlying) => {
            write_type_ref(text, underlying);
            text.push('?');
        }
        TypeRefKind::Array { element, rank } => {
            write_type_ref(text, element);
            text.push('[');
            for _ in 1..*rank {
                text.push(',');
            }
            text.push(']');
        }
        TypeRefKind::Pointer(element) => {
            write_type_ref(text, element);
            text.push('*');
        }
        TypeRefKind::ByRef {
            referent,
            is_readonly,
        } => {
            text.push_str(if *is_readonly { "ref readonly " } else { "ref " });
            write_type_ref(text, referent);
        }
        TypeRefKind::Unbound { parts, arity } => {
            for (index, part) in parts.iter().enumerate() {
                if index > 0 {
                    text.push('.');
                }
                text.push_str(part);
            }
            text.push('<');
            for _ in 1..*arity {
                text.push(',');
            }
            text.push('>');
        }
        TypeRefKind::Error => text.push('?'),
    }
}

/// Whether a conversion operator is implicit or explicit (17.9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionDirection {
    /// `implicit`.
    Implicit,
    /// `explicit`.
    Explicit,
}

impl ConversionDirection {
    /// The metadata method name of a user-defined conversion (II.10.3.3): `op_Implicit`
    /// or `op_Explicit`.
    #[must_use]
    pub fn method_name(self) -> &'static str {
        match self {
            ConversionDirection::Implicit => "op_Implicit",
            ConversionDirection::Explicit => "op_Explicit",
        }
    }
}

/// An operator that may be overloaded by an [`Member::Operator`] (17.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverloadableOperator {
    /// `+`.
    Plus,
    /// `-`.
    Minus,
    /// `!`.
    LogicalNot,
    /// `~`.
    BitwiseNot,
    /// `++`.
    Increment,
    /// `--`.
    Decrement,
    /// `true`.
    True,
    /// `false`.
    False,
    /// `*`.
    Multiply,
    /// `/`.
    Divide,
    /// `%`.
    Remainder,
    /// `&`.
    BitwiseAnd,
    /// `|`.
    BitwiseOr,
    /// `^`.
    ExclusiveOr,
    /// `<<`.
    LeftShift,
    /// `>>`.
    RightShift,
    /// `==`.
    Equality,
    /// `!=`.
    Inequality,
    /// `>`.
    GreaterThan,
    /// `<`.
    LessThan,
    /// `>=`.
    GreaterThanOrEqual,
    /// `<=`.
    LessThanOrEqual,
}

impl OverloadableOperator {
    /// The metadata method name of a user-defined operator (II.10.3.1/2): `op_Addition`,
    /// etc. `+`/`-` are the unary forms with one parameter, the binary forms with two.
    #[must_use]
    pub fn method_name(self, param_count: usize) -> &'static str {
        use OverloadableOperator as O;
        match self {
            O::Plus if param_count == 1 => "op_UnaryPlus",
            O::Plus => "op_Addition",
            O::Minus if param_count == 1 => "op_UnaryNegation",
            O::Minus => "op_Subtraction",
            O::LogicalNot => "op_LogicalNot",
            O::BitwiseNot => "op_OnesComplement",
            O::Increment => "op_Increment",
            O::Decrement => "op_Decrement",
            O::True => "op_True",
            O::False => "op_False",
            O::Multiply => "op_Multiply",
            O::Divide => "op_Division",
            O::Remainder => "op_Modulus",
            O::BitwiseAnd => "op_BitwiseAnd",
            O::BitwiseOr => "op_BitwiseOr",
            O::ExclusiveOr => "op_ExclusiveOr",
            O::LeftShift => "op_LeftShift",
            O::RightShift => "op_RightShift",
            O::Equality => "op_Equality",
            O::Inequality => "op_Inequality",
            O::GreaterThan => "op_GreaterThan",
            O::LessThan => "op_LessThan",
            O::GreaterThanOrEqual => "op_GreaterThanOrEqual",
            O::LessThanOrEqual => "op_LessThanOrEqual",
        }
    }
}

/// A constructor initializer (17.10.1): `: base(args)` or `: this(args)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstructorInitializer {
    /// Whether the initializer calls a base or a sibling constructor.
    pub kind: ConstructorInitializerKind,
    /// The argument expressions.
    pub arguments: Vec<Expr>,
    /// The `base`/`this` keyword's span, for a debug build's sequence point on the chain
    /// call (a breakpoint on `: base(...)` / `: this(...)`).
    pub span: Span,
}

/// Which constructor a [`ConstructorInitializer`] chains to (17.10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstructorInitializerKind {
    /// `: base(...)`.
    Base,
    /// `: this(...)`.
    This,
}

/// A property accessor (17.6.2): a `get` or `set`, with a block body or, for an
/// abstract or interface property, a bare `;` (so the body is `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accessor {
    /// The attributes on the accessor itself (17.6.2 accessor-declarations).
    pub attributes: Vec<AttributeSection>,
    /// The accessor's OWN access modifiers -- `private set`, `protected internal get` (C# 2.0).
    /// Empty when the accessor takes the property's accessibility, which is the common case and
    /// the only one C# 1.0 has. Two entries for the compound `protected internal`.
    ///
    /// Always empty for an EVENT accessor: `add`/`remove` may not carry modifiers (CS1609), and
    /// the event accessor block does not parse any.
    pub modifiers: Vec<Modifier>,
    /// The accessor body, or `None` for a bare `;`.
    pub body: Option<Stmt>,
    /// Whether a property or indexer SETTER was spelled `init` rather than `set` (C# 9).
    ///
    /// **AN `init` ACCESSOR OCCUPIES THE SET SLOT, AND THAT IS MEASURED**: `int P { init { } set
    /// { } }` is `CS1007 Property accessor already defined`, so the two are one accessor spelled
    /// two ways and not two accessors. It is a field here rather than a separate slot on the
    /// property for exactly that reason.
    ///
    /// Always `false` for a GETTER and for an EVENT accessor, the same way [`Accessor::modifiers`]
    /// is always empty for an event's -- the grammar parses no such spelling there.
    ///
    /// What the flag COSTS downstream is a `modreq(System.Runtime.CompilerServices.IsExternalInit)`
    /// on the emitted `set_` accessor's return type, which is the whole of how the distinction
    /// survives into metadata: an init-only setter has an ordinary setter's signature otherwise,
    /// and a reader that drops the modifier cannot tell them apart.
    pub is_init: bool,
    /// The byte range the accessor covers.
    pub span: Span,
}

/// A formal parameter (17.5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    /// The `ref`, `out`, or `params` modifier, if any.
    pub modifier: Option<ParameterModifier>,
    /// The parameter type.
    pub ty: TypeRef,
    /// The parameter name.
    pub name: Box<str>,
    /// The DEFAULT ARGUMENT `= expr` that makes this parameter optional (C# 4.0, 15.6.2.13).
    ///
    /// Carried unevaluated, because whether the expression is a constant of the parameter's type
    /// is a question only the binder can answer -- it needs the resolved type and the enum members
    /// in scope. The parser's job ends at "there was an `= expr` here, and it spans this".
    ///
    /// **Present even at a dialect that forbids it.** The gate is reported and the tree is built
    /// anyway, so the binder still sees a parameter list of the shape the author wrote and a
    /// program does not cascade into a second, unrelated diagnostic on the `=`.
    pub default_value: Option<Expr>,
    /// The byte range the parameter covers.
    pub span: Span,
}

/// One parameter of a lambda expression (14.5.11).
///
/// **SEPARATE FROM [`Parameter`] BECAUSE THE TYPE IS OPTIONAL AND THAT IS THE POINT.** A method
/// parameter always has a written type; a lambda's may be inferred from the target delegate, and
/// modelling that as a `TypeRef` with a placeholder would make "inferred" indistinguishable from a
/// type named by an identifier the binder happens not to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LambdaParameter {
    /// The written type, or `None` when the source left it to be inferred.
    pub ty: Option<TypeRef>,
    /// The parameter name.
    pub name: Box<str>,
    /// The byte range the parameter covers.
    pub span: Span,
}

/// A lambda's body (14.5.11): a single expression, or a block.
///
/// The two are not interchangeable at the binder. An expression body's value IS the return value,
/// so it must convert to the delegate's return type; a block body returns through `return`
/// statements, and a block lambda converted to a `void`-returning delegate must have none that
/// carry a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LambdaBody {
    /// `x => expr`.
    Expression(Expr),
    /// `x => { statements }`.
    Block(Stmt),
}

/// A parameter-passing modifier (17.5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterModifier {
    /// `ref`: pass by reference.
    Ref,
    /// `out`: pass by reference, assigned by the callee.
    Out,
    /// `params`: a variable-length trailing array.
    Params,
}

/// One `catch` clause of a `try` statement (15.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchClause {
    /// The caught exception type, or `None` for a general `catch`.
    pub exception_type: Option<TypeRef>,
    /// The bound exception variable's name, if any.
    pub name: Option<Box<str>>,
    /// The EXCEPTION FILTER's condition -- `catch (E e) when (cond)`, C# 6.0 -- else `None`.
    ///
    /// It is a condition on whether this handler runs AT ALL, which is what separates it from an
    /// `if` at the top of the body: a filter that answers false leaves the exception travelling and
    /// the stack UNWOUND PAST NOTHING, so an outer handler still sees the original throw point.
    /// The exception variable is in scope here, which is the whole reason the clause is worth
    /// writing.
    pub filter: Option<Expr>,
    /// The handler block.
    pub body: Box<Stmt>,
    /// The `catch (...)` clause header's span, for a debug build's sequence point on it.
    pub span: Span,
}

/// The resource of a `using` statement (15.13): a local declaration or an
/// expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsingResource {
    /// `type declarators`.
    Declaration {
        /// The declared type.
        ty: TypeRef,
        /// The declared variables.
        declarators: Vec<VariableDeclarator>,
    },
    /// An expression evaluating to the resource.
    Expression(Expr),
}

/// The initializer of a `for` statement (15.8.3): either a local variable
/// declaration or a list of statement expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForInitializer {
    /// `type declarators`.
    Declaration {
        /// The declared type.
        ty: TypeRef,
        /// The declared variables.
        declarators: Vec<VariableDeclarator>,
    },
    /// A comma-separated list of statement expressions.
    Expressions(Vec<Expr>),
}

/// One declared variable in a [`StmtKind::LocalDeclaration`] (15.5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableDeclarator {
    /// The variable's name.
    pub name: Box<str>,
    /// The initializer expression, if the declarator has one.
    pub initializer: Option<Expr>,
    /// The byte range the declarator covers.
    pub span: Span,
}

/// A literal value as decoded by the lexer (9.4.4): the parser lifts the token's
/// decoded payload into the tree unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    /// An integer literal: its value and the suffix constraining its type.
    Integer {
        /// The numeric value.
        value: u64,
        /// The `U`/`L` suffix, if any.
        suffix: IntegerSuffix,
    },
    /// A real literal: its value as `f64` bits (see [`f64::from_bits`]; stored as
    /// bits so the AST stays `Eq`) and the type suffix.
    Real {
        /// The value's `f64` bit pattern.
        bits: u64,
        /// The `F`/`D`/`M` suffix, if any.
        suffix: RealSuffix,
    },
    /// A character literal: one UTF-16 code unit.
    Character(u16),
    /// A string literal: its decoded UTF-16 code units.
    String(Box<[u16]>),
    /// A boolean literal, `true` or `false`.
    Boolean(bool),
    /// The null literal.
    Null,
    /// A `decimal` (`m`-suffixed) literal, stored EXACTLY as its 96-bit integer mantissa
    /// (`lo`/`mid`/`hi`, value = mantissa x 10^-`scale`), since `f64` cannot represent every
    /// decimal (e.g. `0.1m`) and the scale (`0.10m` vs `0.1m`) must be preserved. `negative` is
    /// set only by folding a unary minus on a literal (`-2.5m`); a bare literal is non-negative.
    Decimal {
        /// Bits 0..32 of the 96-bit mantissa.
        lo: u32,
        /// Bits 32..64 of the mantissa.
        mid: u32,
        /// Bits 64..96 of the mantissa.
        hi: u32,
        /// The power-of-ten scale (0..=28).
        scale: u8,
        /// Whether the value is negated (from folding `-<literal>`).
        negative: bool,
    },
}

/// A prefix unary operator (14.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    /// Unary `+`.
    Plus,
    /// Unary `-`.
    Minus,
    /// Logical negation `!`.
    Not,
    /// Bitwise complement `~`.
    Complement,
    /// Pre-increment `++`.
    PreIncrement,
    /// Pre-decrement `--`.
    PreDecrement,
}

impl UnaryOperator {
    /// The user-defined operator method this unary operator resolves to (II.10.3.2),
    /// or `None` for `++`/`--` (which need lvalue handling) and other non-overloadables.
    #[must_use]
    pub fn overload_method_name(self) -> Option<&'static str> {
        Some(match self {
            UnaryOperator::Plus => "op_UnaryPlus",
            UnaryOperator::Minus => "op_UnaryNegation",
            UnaryOperator::Not => "op_LogicalNot",
            UnaryOperator::Complement => "op_OnesComplement",
            UnaryOperator::PreIncrement | UnaryOperator::PreDecrement => return None,
        })
    }
}

/// A postfix unary operator (14.5.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostfixOperator {
    /// Postfix `++`.
    Increment,
    /// Postfix `--`.
    Decrement,
}

/// A binary operator (14.7 through 14.12). All are left-associative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    /// `*`.
    Multiply,
    /// `/`.
    Divide,
    /// `%`.
    Modulo,
    /// `+`.
    Add,
    /// `-`.
    Subtract,
    /// `<<`.
    LeftShift,
    /// `>>`.
    RightShift,
    /// `<`.
    LessThan,
    /// `>`.
    GreaterThan,
    /// `<=`.
    LessThanOrEqual,
    /// `>=`.
    GreaterThanOrEqual,
    /// `==`.
    Equal,
    /// `!=`.
    NotEqual,
    /// `&`.
    BitwiseAnd,
    /// `^`.
    BitwiseXor,
    /// `|`.
    BitwiseOr,
    /// `&&`.
    LogicalAnd,
    /// `||`.
    LogicalOr,
}

impl BinaryOperator {
    /// The user-defined operator method this binary operator resolves to (II.10.3.1),
    /// or `None` for `&&`/`||`, which are not directly overloadable.
    #[must_use]
    pub fn overload_method_name(self) -> Option<&'static str> {
        use BinaryOperator as B;
        Some(match self {
            B::Multiply => "op_Multiply",
            B::Divide => "op_Division",
            B::Modulo => "op_Modulus",
            B::Add => "op_Addition",
            B::Subtract => "op_Subtraction",
            B::LeftShift => "op_LeftShift",
            B::RightShift => "op_RightShift",
            B::LessThan => "op_LessThan",
            B::GreaterThan => "op_GreaterThan",
            B::LessThanOrEqual => "op_LessThanOrEqual",
            B::GreaterThanOrEqual => "op_GreaterThanOrEqual",
            B::Equal => "op_Equality",
            B::NotEqual => "op_Inequality",
            B::BitwiseAnd => "op_BitwiseAnd",
            B::BitwiseXor => "op_ExclusiveOr",
            B::BitwiseOr => "op_BitwiseOr",
            B::LogicalAnd | B::LogicalOr => return None,
        })
    }
}

/// An assignment operator, simple or compound (14.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentOperator {
    /// `=`.
    Assign,
    /// `+=`.
    Add,
    /// `-=`.
    Subtract,
    /// `*=`.
    Multiply,
    /// `/=`.
    Divide,
    /// `%=`.
    Modulo,
    /// `&=`.
    And,
    /// `|=`.
    Or,
    /// `^=`.
    Xor,
    /// `<<=`.
    LeftShift,
    /// `>>=`.
    RightShift,
}
