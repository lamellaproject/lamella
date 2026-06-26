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
}

/// The kind of an [`Expr`], with any child expressions (ECMA-334 1st ed, 14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    /// A literal value (14.5.1): the lexer decoded it; this carries the result.
    Literal(Literal),
    /// A simple name (14.5.2): a bare identifier, its `@` prefix already removed.
    Name(Box<str>),
    /// A predefined type in expression position (14.5.4): the left side of a
    /// static member access such as `int.Parse`. Binding rejects it anywhere a
    /// value, rather than a type name, is required.
    PredefinedType(PredefinedType),
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
    /// An invocation `receiver(arguments)` (14.5.5).
    Invocation {
        /// The expression being invoked.
        receiver: Box<Expr>,
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
    /// A `ref`/`out` argument (17.5.1): the address of a variable, passed to a byref
    /// parameter. `out` additionally means the callee assigns the variable.
    RefArgument {
        /// `true` for `out`, `false` for `ref`.
        out: bool,
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
    /// A `typeof` expression (14.5.11): `typeof ( type )`.
    TypeOf(TypeRef),
    /// A `sizeof` expression (unsafe, III.4.25): `sizeof ( type )`. Its value is the type's
    /// byte size; for a struct it is the `sizeof` opcode over the shared layout.
    SizeOf(TypeRef),
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
}

impl TypeRef {
    /// Creates a type reference of `kind` covering `span`.
    #[must_use]
    pub fn new(kind: TypeRefKind, span: Span) -> TypeRef {
        TypeRef { kind, span }
    }
}

/// The kind of a [`TypeRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRefKind {
    /// A predefined type keyword, such as `int` or `string` (11.1.4).
    Predefined(PredefinedType),
    /// A type name, its parts in order: `A.B.C` is `["A", "B", "C"]` (11.1).
    Name(Vec<Box<str>>),
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
    /// A placeholder for a type that could not be parsed, emitted with a
    /// diagnostic for recovery.
    Error,
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
    /// A local variable declaration `type declarators ;` (15.5.1).
    LocalDeclaration {
        /// The declared type, shared by every declarator.
        ty: TypeRef,
        /// The declared variables, in order.
        declarators: Vec<VariableDeclarator>,
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

/// A `using` directive (16.3): import a namespace or define an alias.
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
    /// The base class and/or interfaces listed after `:`.
    pub bases: Vec<TypeRef>,
    /// The type's members.
    pub members: Vec<Member>,
    /// The byte range the declaration covers.
    pub span: Span,
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
        /// The formal parameters.
        parameters: Vec<Parameter>,
        /// The method body, or `None` if it was a bare `;`.
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
        /// The `: base(...)` or `: this(...)` initializer, if present.
        initializer: Option<ConstructorInitializer>,
        /// The constructor body.
        body: Stmt,
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
            | Member::Property { attributes: slot, .. }
            | Member::EventField { attributes: slot, .. }
            | Member::Event { attributes: slot, .. } => *slot = attributes,
            _ => {}
        }
    }
}

/// The name an explicit interface member implementation carries in metadata and in
/// the symbol model: the interface's source spelling, a dot, then the member -- e.g.
/// `I.M` or `System.IComparable.CompareTo`. csc names the `MethodDef` this way, and
/// registering it under this mangled name keeps ordinary simple-name lookup of the
/// member from finding it (so it is reachable only through the interface). `member`
/// is the bare member name; `interface_ref` is the qualifying interface type.
#[must_use]
pub fn explicit_interface_member_name(interface_ref: &TypeRef, member: &str) -> String {
    let mut name = String::new();
    if let TypeRefKind::Name(parts) = &interface_ref.kind {
        for part in parts {
            name.push_str(part);
            name.push('.');
        }
    }
    name.push_str(member);
    name
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
    /// The accessor body, or `None` if it was a bare `;`.
    pub body: Option<Stmt>,
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
    /// The byte range the parameter covers.
    pub span: Span,
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
    /// The handler block.
    pub body: Box<Stmt>,
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
