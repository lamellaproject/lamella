//! The parser: building a syntax tree from the token stream.

use crate::ast::{
    Accessor, AssignmentOperator, Attribute, AttributeArgument, AttributeSection, BinaryOperator,
    Argument, ArgumentName,
    TupleElement, TupleElementExpr,
    CatchClause, CompilationUnit, ConstructorInitializer, ConstructorInitializerKind,
    DeconstructionTarget, LambdaBody,
    LambdaParameter,
    ConversionDirection, DelegateDecl, EnumDecl, EnumMember, Expr, ExprKind, ForInitializer,
    GotoTarget, Initializer, InterpolationPart, Literal, Member, MemberInitializer,
    MemberInitializerValue, Modifier,
    NamespaceDecl, NamespaceMember, OverloadableOperator,
    CollectionElement, Parameter, ParameterModifier, PostfixOperator, PredefinedType,
    Pattern, QualifiedName, RecordParts, SwitchArm,
    RefPosition,
    Stmt, StmtKind,
    SwitchLabel, SwitchSection, TypeDecl, TypeKind, TypeParameter, TypeParameterConstraint,
    TypeNamePart, TypeParameterConstraintClause, TypeRef, TypeRefKind,
    TypeTestOperation,
    UnaryOperator, UsingDirective, UsingKind, UsingResource, VariableDeclarator,
};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::lexer::{LexOptions, Tokenized, tokenize, tokenize_with};
use crate::span::Span;
use crate::version::{Feature, FeatureGate, LanguageVersion};
use crate::token::{IntegerSuffix, Keyword, Punctuator, Token, TokenKind, TypedRefKeyword};
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;

/// What a namespace member is being declared inside.
///
/// This exists for the three file-scoped-namespace placement rules (C# 10), each of which turns on
/// the IMMEDIATE container and on nothing else -- see
/// [`Parser::parse_file_scoped_namespace_body`] for the measured table. Threading it through
/// [`Parser::parse_namespace_member`] is what lets those rules be decided where the mistake is,
/// with the offending name in hand, rather than by a second walk over a finished tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamespaceContainer {
    /// The compilation unit (or a REPL submission) itself -- the only place a file-scoped
    /// namespace belongs, and only while nothing else has been declared.
    CompilationUnit {
        /// Whether a type or namespace has already been declared at file scope.
        members_precede: bool,
    },
    /// A brace-delimited `namespace N { ... }` body.
    Block,
    /// A file-scoped `namespace N;` body, which runs to the end of ITS container.
    FileScoped,
}

/// Right-angle brackets a nested type-argument list consumed as part of a `>>` token and owed to
/// the lists enclosing it (25.5.1).
///
/// The lexer takes the longest match (9.4.5), so the two levels of `List<List<int>>` close on ONE
/// [`Punctuator::GreaterThanGreaterThan`]. The inner list consumes that token whole and reports one
/// close still owed; the enclosing list closes on the credit instead of on a token. See
/// [`Parser::parse_type_argument_list`] for why the alternative -- narrowing the token in place --
/// is a silent miscompile rather than a shortcut.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AngleCredit {
    /// How many enclosing type-argument lists may close without consuming a token.
    closes: u8,
    /// The offset past the `>>` these closes came from, so an enclosing list still spans its
    /// source text rather than ending before its own last character.
    end: u32,
}

/// The result of parsing: the syntax tree and every diagnostic gathered, both
/// the lexer's and the parser's, in source order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedExpression {
    /// The expression tree. On error it still parses as much as it can, leaving
    /// [`ExprKind::Error`] placeholders where a subexpression was missing.
    pub expr: Expr,
    /// Lexical and syntactic diagnostics, in source order.
    pub diagnostics: Vec<Diagnostic>,
}

/// Lexes and parses `source` as a single expression (ECMA-334 1st ed, 14).
///
/// Tokens after the expression, if any, are left unconsumed; enforcing that the
/// expression is the entire input belongs to the statement and declaration
/// grammar, which arrives later.
#[must_use]
pub fn parse_expression(source: &str) -> ParsedExpression {
    let mut parser = Parser::new(tokenize(source));
    let expr = parser.parse_expression();
    ParsedExpression {
        expr,
        diagnostics: without_gated_operator_cascades(parser.diagnostics),
    }
}

/// The result of parsing a statement: the tree and every diagnostic gathered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedStatement {
    /// The statement tree, with [`crate::ast::StmtKind::Error`] placeholders
    /// where recovery was needed.
    pub statement: Stmt,
    /// Lexical and syntactic diagnostics, in source order.
    pub diagnostics: Vec<Diagnostic>,
}

/// Lexes and parses `source` as a single statement (ECMA-334 1st ed, clause 15).
#[must_use]
pub fn parse_statement(source: &str) -> ParsedStatement {
    let mut parser = Parser::new(tokenize(source));
    let statement = parser.parse_statement();
    ParsedStatement {
        statement,
        diagnostics: without_gated_operator_cascades(parser.diagnostics),
    }
}

/// The result of parsing a whole compilation unit: the tree and its diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCompilationUnit {
    /// The compilation unit, with `Error` placeholders where recovery was needed.
    pub unit: CompilationUnit,
    /// Lexical and syntactic diagnostics, in source order.
    pub diagnostics: Vec<Diagnostic>,
    /// This file's `#pragma warning disable|restore` directives (9.5.8), in source order. They
    /// govern diagnostics the BINDER produces, so they have to reach whoever holds both.
    pub pragma_warnings: Vec<crate::lexer::PragmaWarning>,
}

/// Lexes and parses `source` as a whole compilation unit (ECMA-334 1st ed, 16.1) under the
/// default dialect (csc-matching, strict ISO-1); use [`parse_compilation_unit_with`] to pass
/// [`LexOptions`] (NFC identifiers, the csc typed-reference operators).
#[must_use]
pub fn parse_compilation_unit(source: &str) -> ParsedCompilationUnit {
    parse_compilation_unit_with(source, LexOptions::default())
}

/// Like [`parse_compilation_unit`], but scans under `options`: identifier folding (9.4.2) and
/// whether the csc typed-reference operators are recognized.
#[must_use]
pub fn parse_compilation_unit_with(
    source: &str,
    options: LexOptions,
) -> ParsedCompilationUnit {
    let version = options.version;
    let tokenized = tokenize_with(source, options);
    let pragma_warnings = tokenized.pragma_warnings.clone();
    let mut parser = Parser::new(tokenized);
    parser.version = version;
    let unit = parser.parse_compilation_unit();
    ParsedCompilationUnit {
        unit,
        diagnostics: without_gated_operator_cascades(parser.diagnostics),
        pragma_warnings,
    }
}

/// The result of parsing a REPL submission: the top-level statement list, the optional
/// trailing display expression, and the diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSubmission {
    /// The submission's leading `using` directives (16.3); the session model accumulates
    /// them so later submissions resolve names without qualification.
    pub usings: Vec<UsingDirective>,
    /// The submission's top-level type and namespace declarations (a `class`/`struct`/
    /// `enum`/etc.); the session emits each as a TypeDef the runtime adds to the module,
    /// and accumulates them so later submissions reference them.
    pub types: Vec<NamespaceMember>,
    /// The submission's top-level statements, in source order. A top-level local
    /// declaration here is a persistent session variable -- the incremental-REPL
    /// session model lowers it to a field of the `__Repl` instance -- with `Error`
    /// placeholders where recovery was needed.
    pub statements: Vec<Stmt>,
    /// A trailing bare expression (no `;`, running to end of input), the submission's
    /// DISPLAY value: the session model returns it boxed to `object` for the REPL to
    /// print. `None` when the submission ends in a statement.
    pub trailing: Option<Expr>,
    /// Lexical and syntactic diagnostics, in source order.
    pub diagnostics: Vec<Diagnostic>,
}

/// Lexes and parses `source` as a REPL submission: leading `using` directives (16.3), a
/// sequence of top-level statements (ECMA-334 1st ed, clause 15), and an optional trailing
/// bare expression (the display value), consumed to end of input. C# 1.0 has no top-level
/// statements in a compilation unit, so this is a REPL-only entry beside [`parse_statement`];
/// the session model binds the result against the persistent `__Repl` scope.
#[must_use]
pub fn parse_submission(source: &str) -> ParsedSubmission {
    let mut parser = Parser::new(tokenize(source));
    let (usings, types, statements, trailing) = parser.parse_submission();
    ParsedSubmission {
        usings,
        types,
        statements,
        trailing,
        diagnostics: without_gated_operator_cascades(parser.diagnostics),
    }
}

/// The source spelling of `kind`, for a diagnostic that names an offending token (e.g. CS1519).
fn token_spelling(kind: &TokenKind) -> Box<str> {
    match kind {
        TokenKind::Identifier(name) => name.clone(),
        TokenKind::Keyword(keyword) => keyword.as_str().into(),
        TokenKind::Punctuator(punctuator) => punctuator.as_str().into(),
        TokenKind::IntegerLiteral { value, .. } => alloc::format!("{value}").into(),
        _ => "?".into(),
    }
}

/// What a `ref` in front of a declarator's initializer means at the position being parsed --
/// [`Parser::parse_variable_declarators`] serves five of them and csc answers differently at
/// three.
///
/// **THE VARIANT IS THE POSITION, NOT A PERMISSION BIT**, because two of the three still ACCEPT
/// the spelling and differ only in whether the rung gate fires there. A bare `allow: bool` would
/// have to be paired with a second bool for the gate, and a call site passing `(true, false)`
/// says nothing about which case it is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InitializerRefs {
    /// A field, an event field, a `using` resource, or a local CONSTANT: `ref` is not the grammar
    /// here at any rung, so it is left to fail as the token it is.
    Forbidden,
    /// A local declaration whose type is by VALUE -- `int r = ref a[0];`. csc parses it and
    /// answers CS8171 (*Cannot initialize a by-value variable with a reference*), and the rung
    /// gate fires HERE because the declaration has no `ref` of its own to have fired at.
    ByValueLocal,
    /// A local declaration whose type is by REFERENCE -- `ref int r = ref a[0];`. The expected
    /// form, and the gate has already fired at the declaration's own `ref`.
    ByRefLocal,
}

/// A recursive-descent parser over a filtered token stream.
struct Parser {
    /// The significant tokens, trivia removed, always ending in `EndOfFile`.
    tokens: Vec<Token>,
    /// The index of the token currently being looked at.
    position: usize,
    /// Diagnostics gathered so far, beginning with the lexer's.
    diagnostics: Vec<Diagnostic>,
    /// The `#define`d preprocessor symbols (9.5.3), carried onto the parsed
    /// [`CompilationUnit`] so the binder can resolve `[Conditional]` inclusion (24.4.2).
    defined_symbols: BTreeSet<Box<str>>,
    /// The dialect being compiled. The parser gates several constructs on it, and the gate
    /// DIAGNOSTIC needs it too -- its code and its "in C# N" both name the version being compiled,
    /// not the one the feature wants.
    version: LanguageVersion,
    /// Whether the parser is inside the parameter list or body of a method whose modifiers
    /// include `async`. This is the CONTEXT the contextuality of `await` is scoped to (ECMA-334
    /// 5th ed, 12.8.8.1): inside, every non-verbatim `await` is the operator and a declared name
    /// spelled `await` is CS4003; outside, `await` is an ordinary identifier except in the
    /// measured operator shapes (see [`Parser::await_operator_here`]). Saved and restored around
    /// each method, never around nested types -- a nested type's members parse through their own
    /// `parse_member` calls, each of which sets it from its own modifiers.
    in_async_method: bool,
    /// Whether the member whose body is being parsed returns BY REFERENCE, so `return ref e;`
    /// inside it does not gate the feature a second time.
    ///
    /// **csc REPORTS THE RUNG ONCE PER MEMBER, AT THE DECLARATION, AND THIS FLAG IS THE ONLY
    /// REASON WE DO TOO.** Measured at `/langversion:6`: `ref int M() { return ref a[0]; }` draws
    /// ONE CS8059, at the declaration's `ref` -- the body's `ref` draws nothing, because the
    /// declaration gate already fired for that member. The same body in a BY-VALUE method draws
    /// the gate at the body's `ref` instead, since nothing gated ahead of it. Without this,
    /// the first program reports twice and diverges from csc on a count.
    in_ref_returning_member: bool,
    /// Whether the parser is inside an `unsafe { ... }` block, which the parser LOWERS to a
    /// plain block (pointer use is permitted regardless) -- so the one rule that needs the
    /// context, `await` refusing there (CS4004, measured), must be raised while the block is
    /// still visible. Tracked only inside async methods, where the operator exists.
    in_unsafe_block: bool,
}

/// Which token, after the closing bracket, settles a bracketed list as a deconstruction: `=` in
/// an expression, `in` in a `foreach` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeconstructionTerminator {
    /// `(a, b) = t`
    Assign,
    /// `foreach ((a, b) in e)`
    In,
}

impl Parser {
    /// Creates a parser over a lexed source, dropping trivia and keeping the
    /// lexer's diagnostics so the two stages report through one channel.
    fn new(tokenized: Tokenized) -> Parser {
        let tokens = tokenized
            .tokens
            .into_iter()
            .filter(|token| !token.is_trivia())
            .collect();
        Parser {
            tokens,
            position: 0,
            diagnostics: tokenized.diagnostics,
            defined_symbols: tokenized.defined_symbols,
            version: LanguageVersion::DEFAULT,
            in_async_method: false,
            in_ref_returning_member: false,
            in_unsafe_block: false,
        }
    }

    /// The token currently being looked at. Never past the end: the final
    /// `EndOfFile` token is returned once the stream is exhausted.
    fn current(&self) -> &Token {
        &self.tokens[self.position]
    }

    /// Advances to the next token, stopping on the final `EndOfFile`.
    fn bump(&mut self) {
        if self.position + 1 < self.tokens.len() {
            self.position += 1;
        }
    }

    /// The current token's punctuator, if it is one.
    fn current_punctuator(&self) -> Option<Punctuator> {
        match self.current().kind {
            TokenKind::Punctuator(punctuator) => Some(punctuator),
            _ => None,
        }
    }

    /// Consumes the current token if it is `punctuator`, reporting whether it was.
    fn eat(&mut self, punctuator: Punctuator) -> bool {
        if self.current_punctuator() == Some(punctuator) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// The current token's keyword, if it is one.
    fn current_keyword(&self) -> Option<Keyword> {
        match self.current().kind {
            TokenKind::Keyword(keyword) => Some(keyword),
            _ => None,
        }
    }

    /// The current token's identifier text, if it is an identifier.
    ///
    /// **ASK [`Parser::current_contextual_keyword`] INSTEAD WHENEVER THE TEXT IS COMPARED AGAINST A
    /// CONTEXTUAL KEYWORD** -- `get`, `set`, `add`, `remove`, `where`, `required`, `async`,
    /// `await`. This one answers about the NAME, which is the right question for a declared
    /// identifier and the wrong one for a word that is only sometimes a keyword.
    fn current_identifier_text(&self) -> Option<&str> {
        match &self.current().kind {
            TokenKind::Identifier(text) => Some(text),
            _ => None,
        }
    }

    /// The current token's identifier text WHEN IT MAY BE READ AS A CONTEXTUAL KEYWORD -- `None`
    /// for a verbatim identifier, which 9.4.2 forces back to an ordinary name.
    ///
    /// **ONE FUNCTION BECAUSE THE RULE HAS ONE STATEMENT AND SEVEN SITES.** `@get` is a method
    /// named `get`, `@where` is a type named `where`, `@required` is a type named `required`, and
    /// each is a program csc REJECTS -- so a site that compares the text alone accepts something
    /// invalid, quietly, and every new contextual keyword adds another such site. Measured before
    /// the fix: `class Box<T> @where T : class` compiled here and is CS1514 in csc, and
    /// `@required int n;` was read as the modifier where csc reads a type name (CS1519).
    fn current_contextual_keyword(&self) -> Option<&str> {
        if self.current().verbatim {
            return None;
        }
        self.current_identifier_text()
    }

    /// Consumes the current token if it is `keyword`, reporting whether it was.
    fn eat_keyword(&mut self, keyword: Keyword) -> bool {
        if self.current_keyword() == Some(keyword) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Requires `keyword`, reporting `'expected' expected` at the current
    /// position if it is absent.
    fn expect_keyword(&mut self, keyword: Keyword, expected: &'static str) {
        if !self.eat_keyword(keyword) {
            let at = self.current().span.start;
            self.report(
                DiagnosticKind::TokenExpected { expected },
                Span::empty_at(at),
            );
        }
    }

    /// Whether the token after the current one is `punctuator`.
    /// Whether the `?` at the cursor -- which follows an `is` target -- opens a NULLABLE PATTERN
    /// TYPE rather than the conditional operator. See the call site for why three tokens are
    /// needed: `? IDENT :` is a conditional and `? IDENT` anything else is a designator.
    fn at_nullable_pattern_type(&self) -> bool {
        let after = self.tokens.get(self.position + 1).map(|token| &token.kind);
        if !matches!(after, Some(TokenKind::Identifier(_))) {
            return false;
        }
        !matches!(
            self.tokens.get(self.position + 2).map(|token| &token.kind),
            Some(TokenKind::Punctuator(Punctuator::Colon))
        )
    }

    fn next_is(&self, punctuator: Punctuator) -> bool {
        matches!(
            self.tokens.get(self.position + 1).map(|token| &token.kind),
            Some(TokenKind::Punctuator(found)) if *found == punctuator
        )
    }

    /// Requires `punctuator`, returning the byte offset just past it. When it is
    /// absent, `missing` is reported at the current position and that position is
    /// returned, so a caller's span computation still terminates the node.
    fn expect(&mut self, punctuator: Punctuator, missing: DiagnosticKind) -> u32 {
        if self.current_punctuator() == Some(punctuator) {
            let end = self.current().span.end;
            self.bump();
            end
        } else {
            let at = self.current().span.start;
            self.report(missing, Span::empty_at(at));
            at
        }
    }

    fn report(&mut self, kind: DiagnosticKind, span: Span) {
        self.diagnostics.push(Diagnostic::new(kind, span));
    }

    /// Parses a statement (clause 15): a block, the empty statement, `return`,
    /// `if`, `while`, a local declaration, or an expression statement.
    fn parse_statement(&mut self) -> Stmt {
        let start = self.current().span.start;
        if self.current_punctuator() == Some(Punctuator::OpenBrace) {
            return self.parse_block();
        }
        if self.current_punctuator() == Some(Punctuator::Semicolon) {
            let end = self.current().span.end;
            self.bump();
            return Stmt::new(StmtKind::Empty, Span::new(start, end));
        }
        if let Some(keyword) = self.current_keyword() {
            match keyword {
                Keyword::Return => return self.parse_return(start),
                Keyword::If => return self.parse_if(start),
                Keyword::While => return self.parse_while(start),
                Keyword::Do => return self.parse_do_while(start),
                Keyword::For => return self.parse_for(start),
                Keyword::Foreach => return self.parse_foreach(start),
                Keyword::Break => return self.parse_keyword_then_semicolon(start, StmtKind::Break),
                Keyword::Continue => {
                    return self.parse_keyword_then_semicolon(start, StmtKind::Continue);
                }
                Keyword::Throw => return self.parse_throw(start),
                Keyword::Try => return self.parse_try(start),
                Keyword::Lock => return self.parse_lock(start),
                Keyword::Using => return self.parse_using(start),
                Keyword::Fixed => return self.parse_fixed(start),
                Keyword::Switch => return self.parse_switch(start),
                Keyword::Goto => return self.parse_goto(start),
                Keyword::Checked | Keyword::Unchecked if self.next_is(Punctuator::OpenBrace) => {
                    return self.parse_checked_block(start, keyword);
                }
                Keyword::Unsafe if self.next_is(Punctuator::OpenBrace) => {
                    self.bump();
                    let was_unsafe = self.in_unsafe_block;
                    self.in_unsafe_block = true;
                    let block = self.parse_block();
                    self.in_unsafe_block = was_unsafe;
                    return block;
                }
                _ => {}
            }
        }
        if matches!(self.current().kind, TokenKind::Identifier(_))
            && self.next_is(Punctuator::Colon)
        {
            return self.parse_labeled(start);
        }
        self.parse_declaration_or_expression_statement(start)
    }

    /// Parses a REPL submission: leading `using` directives (16.3), then top-level
    /// statements until end of input (15), then an optional trailing bare expression (one
    /// with no `;`, running to end of input) returned separately as the submission's
    /// DISPLAY value -- C# interactive semantics, where `x * 2` prints its value but
    /// `x * 2;` does not. Mirrors [`Parser::parse_block`]'s statement loop (no closing
    /// brace, the same no-progress guard).
    fn parse_submission(
        &mut self,
    ) -> (Vec<UsingDirective>, Vec<NamespaceMember>, Vec<Stmt>, Option<Expr>) {
        let mut usings = Vec::new();
        while self.current_keyword() == Some(Keyword::Using) && !self.next_is(Punctuator::OpenParen)
        {
            usings.push(self.parse_using_directive());
        }
        let mut types = Vec::new();
        let mut statements = Vec::new();
        let mut trailing = None;
        while !matches!(self.current().kind, TokenKind::EndOfFile) {
            if self.at_namespace_member() {
                types.push(self.parse_namespace_member(NamespaceContainer::CompilationUnit {
                    members_precede: !types.is_empty() || !statements.is_empty(),
                }));
                continue;
            }
            let saved_position = self.position;
            let saved_diagnostics = self.diagnostics.len();
            let expr = self.parse_expression();
            if matches!(self.current().kind, TokenKind::EndOfFile)
                && !matches!(expr.kind, ExprKind::Error)
            {
                trailing = Some(expr);
                break;
            }
            self.position = saved_position;
            self.diagnostics.truncate(saved_diagnostics);

            let before = self.position;
            statements.push(self.parse_statement());
            if self.position == before {
                self.bump();
            }
        }
        (usings, types, statements, trailing)
    }

    /// Whether the current position begins a namespace member -- a `namespace`, or, past
    /// any attributes and modifiers, a `class`/`struct`/`interface`/`enum`/`delegate`
    /// keyword -- rather than a statement. A leading modifier alone is not enough, since
    /// `const int x = 5;` is a local declaration; the speculative skip (fully backtracked)
    /// looks for the type-kind keyword behind the modifiers.
    fn at_namespace_member(&mut self) -> bool {
        if self.current_keyword() == Some(Keyword::Namespace) {
            return true;
        }
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        let _ = self.parse_attribute_sections();
        let _ = self.parse_modifiers();
        let is_type = matches!(
            self.current_keyword(),
            Some(
                Keyword::Class
                    | Keyword::Struct
                    | Keyword::Interface
                    | Keyword::Enum
                    | Keyword::Delegate
            )
        );
        self.position = saved_position;
        self.diagnostics.truncate(saved_diagnostics);
        is_type
    }

    /// Parses a block `{ statements }` (15.2), with the scanner at the `{`.
    fn parse_block(&mut self) -> Stmt {
        let start = self.current().span.start;
        self.bump();
        let statements = self.parse_block_statements();
        let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
        Stmt::new(StmtKind::Block(statements), Span::new(start, end))
    }

    /// The statements of a block, up to but not including its `}`.
    ///
    /// **SPLIT FROM [`Parser::parse_block`] SO A USING DECLARATION CAN TAKE THE REST OF THE BLOCK**
    /// -- `using T x = e;` means `using (T x = e) { <every statement after it> }`, so the construct
    /// needs the tail of the list it is a member of, which the statement parser cannot see and this
    /// loop can. It recurses, which is what makes several in one block fall out for free: the
    /// second declaration is simply the first statement of the first's body.
    fn parse_block_statements(&mut self) -> Vec<Stmt> {
        let mut statements = Vec::new();
        while self.current_punctuator() != Some(Punctuator::CloseBrace)
            && !matches!(self.current().kind, TokenKind::EndOfFile)
        {
            if self.using_declaration_here() {
                statements.push(self.parse_using_declaration());
                break;
            }
            let before = self.position;
            statements.push(self.parse_statement());
            if self.position == before {
                self.bump();
            }
        }
        statements
    }

    /// Whether a USING DECLARATION starts here (C# 8.0) rather than a `using` STATEMENT.
    ///
    /// **ONE TOKEN SEPARATES THEM AND NOTHING ELSE CAN**: a `using` statement's resource is
    /// parenthesized and a declaration's is not, so a `(` after the keyword is the statement and
    /// anything else is the declaration. There is no third form -- a using DIRECTIVE cannot appear
    /// where a statement can.
    fn using_declaration_here(&self) -> bool {
        self.current_keyword() == Some(Keyword::Using) && !self.next_is(Punctuator::OpenParen)
    }

    /// Parses `using T x = e;` and REWRITES IT INTO THE `using` STATEMENT IT MEANS, taking the rest
    /// of the enclosing block as its body (C# 8.0).
    ///
    /// **THE DESUGAR IS MEASURED AGAINST csc, NOT REASONED FROM THE SPEC TEXT.** Eight pairs, each
    /// the declaration form beside the hand-written `using (T x = e) { rest }`, compiled by csc and
    /// compared as IL: under `/optimize+` seven are BYTE-IDENTICAL and the eighth differs only in
    /// which local SLOT NUMBER the variable takes -- the declaration form declares it in the
    /// enclosing block's scope, the statement form opens a new one, so a variable declared before
    /// it is numbered differently. The `try`/`finally`, the null test, the `Dispose` call and the
    /// `leave` are the same instructions in the same order.
    ///
    ///
    /// So nothing after the parser learns a new shape, exactly as an expression body works.
    ///
    fn parse_using_declaration(&mut self) -> Stmt {
        let start = self.current().span.start;
        self.bump();
        self.gate_feature(Feature::UsingDeclaration, Span::empty_at(start));
        let resource = self.parse_using_resource();
        self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        let body_start = self.current().span.start;
        let statements = self.parse_block_statements();
        let body_end = statements.last().map_or(body_start, |last| last.span.end);
        let body = Stmt::new(
            StmtKind::Block(statements),
            Span::new(body_start, body_end),
        );
        Stmt::new(
            StmtKind::Using {
                resource,
                body: Box::new(body),
            },
            Span::new(start, body_end),
        )
    }

    /// Parses a `return` statement (15.9.4): `return expression_opt ;`.
    fn parse_return(&mut self, start: u32) -> Stmt {
        self.bump();
        let value = if self.current_punctuator() == Some(Punctuator::Semicolon) {
            None
        } else if self.current_keyword() == Some(Keyword::Ref) {
            let at = self.current().span;
            self.bump();
            if !self.in_ref_returning_member {
                self.gate_feature(Feature::ByRefLocalsAndReturns, at);
            }
            let operand = self.parse_expression();
            let span = Span::new(at.start, operand.span.end);
            Some(Expr::new(
                ExprKind::RefArgument {
                    position: RefPosition::Return,
                    out: false,
                    operand: Box::new(operand),
                },
                span,
            ))
        } else {
            Some(self.parse_expression())
        };
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        Stmt::new(StmtKind::Return(value), Span::new(start, end))
    }

    /// Parses an `if` statement (15.7.1): `if ( expression ) statement` with an
    /// optional `else statement`. An `else` binds to the nearest `if`.
    fn parse_if(&mut self, start: u32) -> Stmt {
        self.bump();
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        let condition = self.parse_expression();
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        let then_branch = Box::new(self.parse_statement());
        let mut end = then_branch.span.end;
        let else_branch = if self.eat_keyword(Keyword::Else) {
            let statement = self.parse_statement();
            end = statement.span.end;
            Some(Box::new(statement))
        } else {
            None
        };
        Stmt::new(
            StmtKind::If {
                condition,
                then_branch,
                else_branch,
            },
            Span::new(start, end),
        )
    }

    /// Parses a `while` statement (15.8.1): `while ( expression ) statement`.
    fn parse_while(&mut self, start: u32) -> Stmt {
        self.bump();
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        let condition = self.parse_expression();
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        let body = Box::new(self.parse_statement());
        let end = body.span.end;
        Stmt::new(StmtKind::While { condition, body }, Span::new(start, end))
    }

    /// Disambiguates a local declaration from an expression statement (15.5.1,
    /// 15.6): the statement is a declaration when it begins with a type followed
    /// by an identifier (the variable name). The type is parsed speculatively and
    /// rolled back, diagnostics included, if it turns out to be an expression.
    fn parse_declaration_or_expression_statement(&mut self, start: u32) -> Stmt {
        if self.current_keyword() == Some(Keyword::Const) {
            self.bump();
            let ty = self.parse_type();
            return self.parse_local_declaration(start, ty, true);
        }
        if self.current_keyword() == Some(Keyword::Ref) {
            let ty = self.parse_ref_local_type();
            return self.parse_local_declaration(start, ty, false);
        }
        if self.await_blocks_declaration_here() {
            return self.parse_expression_statement(start);
        }
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        let ty = self.parse_type();
        if !matches!(ty.kind, TypeRefKind::Error)
            && matches!(self.current().kind, TokenKind::Identifier(_))
        {
            return self.parse_local_declaration(start, ty, false);
        }
        self.position = saved_position;
        self.diagnostics.truncate(saved_diagnostics);
        self.parse_expression_statement(start)
    }

    /// An OUT VARIABLE DECLARATION, `out int a` / `out var a` (C# 7.0), or `None` when the `out`
    /// names an existing variable and the caller should parse an ordinary expression.
    ///
    /// The disambiguation is [`Parser::parse_declaration_or_expression_statement`]'s, for the same
    /// reason: a TYPE FOLLOWED BY AN IDENTIFIER is a declaration and nothing else is, and only a
    /// speculative parse can tell -- `out a` and `out A a` share their first token. Position AND
    /// diagnostics roll back together, so a rejected reading reports nothing.
    ///
    fn parse_out_variable_declaration(&mut self) -> Option<Expr> {
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        let start = self.current().span.start;
        let ty = self.parse_type();
        if !matches!(ty.kind, TypeRefKind::Error) {
            if let TokenKind::Identifier(name) = &self.current().kind {
                let name = name.clone();
                let end = self.current().span.end;
                self.bump();
                let span = Span::new(start, end);
                self.gate_feature(Feature::OutVariableDeclaration, span);
                return Some(Expr::new(ExprKind::DeclarationExpression { ty, name }, span));
            }
        }
        self.position = saved_position;
        self.diagnostics.truncate(saved_diagnostics);
        None
    }

    /// A DECONSTRUCTION -- `(a, b) = t`, `(int a, int b) = t`, `var (a, b) = t` (C# 7.0) -- or
    /// `None` when the brackets belong to something else and the caller should parse an ordinary
    /// expression.
    ///
    /// **THE DECIDING TOKEN IS THE `=` AFTER THE CLOSING BRACKET, WHICH IS NOT VISIBLE UNTIL THE
    /// LIST HAS BEEN PARSED.** So the list is SPECULATED and rewound, the same machinery
    /// [`Parser::parse_out_variable_declaration`] and the `new (T, T)[n]` arm in
    /// [`Parser::parse_new`] use. Position and diagnostics roll back together, so a rejected
    /// reading reports nothing and `(a, b)` goes on being a tuple.
    ///
    /// **`==` IS NOT `=`.** `(a, b) == t` is tuple equality (C# 7.3), a different feature with a
    /// different meaning, and the punctuator table has them as distinct tokens -- so testing for
    /// [`Punctuator::Equals`] admits one and rejects the other with no lookahead of its own. A
    /// compound `+=` is refused for the same reason: C# gives it no deconstructing form.
    fn try_parse_deconstruction(&mut self) -> Option<Expr> {
        let start = self.current().span.start;
        let designated = matches!(self.current_contextual_keyword(), Some("var"))
            && self.next_is(Punctuator::OpenParen);
        if !designated && self.current_punctuator() != Some(Punctuator::OpenParen) {
            return None;
        }
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        let var_span = if designated {
            let at = self.current().span;
            self.bump();
            Some(at)
        } else {
            None
        };
        if !self.deconstruction_ahead(DeconstructionTerminator::Assign) {
            self.position = saved_position;
            return None;
        }
        let bracket = self.current().span.start;
        let parsed = self.try_parse_deconstruction_targets(designated);
        if let Some((targets, _)) = parsed
            && self.current_punctuator() == Some(Punctuator::Equals)
        {
            self.gate_feature(Feature::Tuples, Span::empty_at(bracket));
            self.bump();
            let value = self.parse_expression();
            let end = value.span.end;
            return Some(Expr::new(
                ExprKind::Deconstruction {
                    var_span,
                    targets,
                    value: Box::new(value),
                },
                Span::new(start, end),
            ));
        }
        self.position = saved_position;
        self.diagnostics.truncate(saved_diagnostics);
        None
    }

    /// Whether the brackets at the current `(` could possibly be a DECONSTRUCTION TARGET LIST --
    /// a token scan with no parsing in it, run BEFORE any speculation.
    ///
    /// **THE SPECULATION IT GUARDS IS EXPONENTIAL IN THE BRACKET NESTING WITHOUT IT.** Trying the
    /// target list at every `(` means parsing the bracketed expression once to reject it and again
    /// to keep it -- and the rejected parse descends through `parse_conditional` back into
    /// `parse_assignment`, which speculates again at the next level down. Work doubles per level,
    /// so a deeply nested expression does not finish.
    ///
    /// TWO NECESSARY CONDITIONS, BOTH CHEAP. A target list holds AT LEAST TWO targets, so it has a
    /// comma at its own bracket depth; and it is followed by the token that decides the reading --
    /// `=` for the assignment, `in` for the `foreach` header. `(((1)))` has no comma and stops at
    /// the first condition; `(a, b)` as a tuple expression has the comma and fails the second.
    ///
    /// **PERMISSIVE ON PURPOSE.** A comma at depth one may belong to something else -- a generic
    /// argument list, whose `<` and `>` are not brackets here -- and a false positive costs only
    /// the speculation. A false NEGATIVE would silently unbuild the feature, so every condition
    /// here is one a real deconstruction must satisfy.
    ///
    fn deconstruction_ahead(&self, terminator: DeconstructionTerminator) -> bool {
        let mut depth = 0usize;
        let mut comma_at_top = false;
        let mut index = self.position;
        while let Some(token) = self.tokens.get(index) {
            match &token.kind {
                TokenKind::Punctuator(
                    Punctuator::OpenParen | Punctuator::OpenBracket | Punctuator::OpenBrace,
                ) => depth += 1,
                TokenKind::Punctuator(
                    Punctuator::CloseParen | Punctuator::CloseBracket | Punctuator::CloseBrace,
                ) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        if !comma_at_top {
                            return false;
                        }
                        return match (terminator, self.tokens.get(index + 1).map(|t| &t.kind)) {
                            (
                                DeconstructionTerminator::Assign,
                                Some(TokenKind::Punctuator(Punctuator::Equals)),
                            ) => true,
                            (
                                DeconstructionTerminator::In,
                                Some(TokenKind::Keyword(Keyword::In)),
                            ) => true,
                            _ => false,
                        };
                    }
                }
                TokenKind::Punctuator(Punctuator::Comma) if depth == 1 => comma_at_top = true,
                TokenKind::EndOfFile => return false,
                _ => {}
            }
            index += 1;
        }
        false
    }

    /// A bracketed DECONSTRUCTION TARGET LIST, with the scanner on the `(`; the targets and the
    /// offset just past the closing bracket, or `None` if this is not one.
    ///
    /// `designated` is the `var (...)` form, in which every leaf is a DECLARATION with no written
    /// type. That is not a shorthand this collapses for convenience -- it is what csc means by it:
    /// `int a = 0; var (b, a) = (2, 40);` is `CS0128`, a duplicate local, and not an assignment to
    /// the `a` already in scope (measured). So `var` distributes to every leaf, at every depth,
    /// and recording a declaration at each one loses nothing.
    ///
    /// **AT LEAST TWO TARGETS, MATCHING THE TUPLE RULE ABOVE IT.** `(int a) = t` is `CS1073` from
    /// csc rather than a one-element deconstruction, so a one-element list is not this production
    /// and the caller rewinds to find whatever it really is.
    fn try_parse_deconstruction_targets(
        &mut self,
        designated: bool,
    ) -> Option<(Vec<DeconstructionTarget>, u32)> {
        if !self.eat(Punctuator::OpenParen) {
            return None;
        }
        let mut targets = Vec::new();
        loop {
            targets.push(self.try_parse_deconstruction_target(designated)?);
            if !self.eat(Punctuator::Comma) {
                break;
            }
        }
        if targets.len() < 2 || self.current_punctuator() != Some(Punctuator::CloseParen) {
            return None;
        }
        let end = self.current().span.end;
        self.bump();
        Some((targets, end))
    }

    /// One target: a nested list, a discard, a declaration, or an assignable expression.
    fn try_parse_deconstruction_target(
        &mut self,
        designated: bool,
    ) -> Option<DeconstructionTarget> {
        let start = self.current().span.start;
        if self.current_punctuator() == Some(Punctuator::OpenParen) {
            let saved_position = self.position;
            let saved_diagnostics = self.diagnostics.len();
            if let Some((targets, end)) = self.try_parse_deconstruction_targets(designated) {
                return Some(DeconstructionTarget::Nested {
                    targets,
                    span: Span::new(start, end),
                });
            }
            self.position = saved_position;
            self.diagnostics.truncate(saved_diagnostics);
            if designated {
                return None;
            }
        }
        if designated {
            let span = self.current().span;
            let TokenKind::Identifier(name) = &self.current().kind else {
                return None;
            };
            let name = name.clone();
            let verbatim = self.current().verbatim;
            self.bump();
            if &*name == "_" && !verbatim {
                return Some(DeconstructionTarget::Discard(span));
            }
            return Some(DeconstructionTarget::Declaration {
                ty: None,
                name,
                span,
            });
        }
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        let ty = self.parse_type();
        if !matches!(ty.kind, TypeRefKind::Error)
            && let TokenKind::Identifier(name) = &self.current().kind
        {
            let name = name.clone();
            let end = self.current().span.end;
            self.bump();
            let ty = if matches!(&ty.kind, TypeRefKind::Name(parts) if parts.len() == 1 && &*parts[0] == "var")
            {
                None
            } else {
                Some(ty)
            };
            return Some(DeconstructionTarget::Declaration {
                ty,
                name,
                span: Span::new(start, end),
            });
        }
        self.position = saved_position;
        self.diagnostics.truncate(saved_diagnostics);
        if matches!(self.current_contextual_keyword(), Some("_"))
            && (self.next_is(Punctuator::Comma) || self.next_is(Punctuator::CloseParen))
        {
            self.gate_feature(Feature::Discards, self.current().span);
        }
        let target = self.parse_conditional();
        if matches!(target.kind, ExprKind::Error) {
            return None;
        }
        Some(DeconstructionTarget::Expression(target))
    }

    /// Whether a statement beginning at the current non-verbatim `await` must take the
    /// EXPRESSION reading (see the carve-out in
    /// [`Parser::parse_declaration_or_expression_statement`]). Inside an async method: always.
    /// Outside one: only for `await Ident (`, where the declaration reading is impossible and
    /// csc reads the operator (measured).
    fn await_blocks_declaration_here(&self) -> bool {
        if !matches!(self.current_contextual_keyword(), Some("await")) {
            return false;
        }
        if self.in_async_method {
            return true;
        }
        matches!(
            self.tokens.get(self.position + 1).map(|token| &token.kind),
            Some(TokenKind::Identifier(_))
        ) && matches!(
            self.tokens.get(self.position + 2).map(|token| &token.kind),
            Some(TokenKind::Punctuator(Punctuator::OpenParen))
        )
    }

    /// Whether the current token begins an `await` OPERATOR in expression position.
    ///
    /// Inside an async method: every non-verbatim `await` (the word is reserved there,
    /// 12.8.8.1). Outside one, only the shapes csc reads as the operator, each measured
    /// one compilation apiece: a following identifier (`x = await T()` is CS4033), a literal, or a
    /// token that begins a primary expression and cannot continue `await`-as-identifier
    /// (`new`, `this`, `base`, `typeof`, `true`/`false`/`null`, a predefined type). NOT `(`
    /// (`await(1)` stays a call of a method named `await` -- compiles in csc), NOT an
    /// operator (`await + 1` stays identifier-plus-plus -- compiles), NOT `[`, `.`, `;`.
    fn await_operator_here(&self) -> bool {
        if !matches!(self.current_contextual_keyword(), Some("await")) {
            return false;
        }
        if self.in_async_method {
            return true;
        }
        match self.tokens.get(self.position + 1).map(|token| &token.kind) {
            Some(
                TokenKind::Identifier(_)
                | TokenKind::IntegerLiteral { .. }
                | TokenKind::RealLiteral { .. }
                | TokenKind::DecimalLiteral { .. }
                | TokenKind::CharacterLiteral(_)
                | TokenKind::StringLiteral(_),
            ) => true,
            Some(TokenKind::Keyword(keyword)) => {
                matches!(
                    keyword,
                    Keyword::New
                        | Keyword::This
                        | Keyword::Base
                        | Keyword::Typeof
                        | Keyword::True
                        | Keyword::False
                        | Keyword::Null
                ) || predefined_type(&TokenKind::Keyword(*keyword)).is_some()
            }
            _ => false,
        }
    }

    /// Parses one or more comma-separated variable declarators (15.5.1), each an
    /// identifier with an optional `= expression` initializer. Array initializers
    /// are not yet parsed. Does not consume a terminator.
    ///
    /// `refs` says what a `ref` in front of an initializer means at this position; see
    /// [`InitializerRefs`], whose three cases are three different csc answers.
    fn parse_variable_declarators(&mut self, refs: InitializerRefs) -> Vec<VariableDeclarator> {
        let mut declarators = Vec::new();
        loop {
            let declarator_start = self.current().span.start;
            let (name, mut end) = self.expect_declared_name();
            let initializer = if self.eat(Punctuator::Equals) {
                let value = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                    self.parse_array_initializer()
                } else if refs != InitializerRefs::Forbidden
                    && self.current_keyword() == Some(Keyword::Ref)
                {
                    let at = self.current().span;
                    self.bump();
                    if refs == InitializerRefs::ByValueLocal {
                        self.gate_feature(Feature::ByRefLocalsAndReturns, at);
                    }
                    let operand = self.parse_expression();
                    let span = Span::new(at.start, operand.span.end);
                    Expr::new(
                        ExprKind::RefArgument {
                            position: RefPosition::Argument,
                            out: false,
                            operand: Box::new(operand),
                        },
                        span,
                    )
                } else {
                    self.parse_expression()
                };
                end = value.span.end;
                Some(value)
            } else {
                None
            };
            declarators.push(VariableDeclarator {
                name,
                initializer,
                span: Span::new(declarator_start, end),
            });
            if !self.eat(Punctuator::Comma) {
                break;
            }
        }
        declarators
    }

    /// Parses the declarators and terminator of a local declaration, given its
    /// already-parsed type (15.5.1).
    fn parse_local_declaration(&mut self, start: u32, ty: TypeRef, is_const: bool) -> Stmt {
        let refs = match (&ty.kind, is_const) {
            (_, true) => InitializerRefs::Forbidden,
            (TypeRefKind::ByRef { .. }, false) => InitializerRefs::ByRefLocal,
            (_, false) => InitializerRefs::ByValueLocal,
        };
        let declarators = self.parse_variable_declarators(refs);
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        Stmt::new(
            StmtKind::LocalDeclaration {
                ty,
                declarators,
                is_const,
            },
            Span::new(start, end),
        )
    }

    /// Parses an expression statement (15.6): `expression ;`.
    fn parse_expression_statement(&mut self, start: u32) -> Stmt {
        let expr = self.parse_expression();
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        Stmt::new(StmtKind::Expression(expr), Span::new(start, end))
    }

    /// A comma-separated list of expressions (14.4.1), used by `for` clauses.
    fn parse_expression_list(&mut self) -> Vec<Expr> {
        let mut expressions = Vec::new();
        expressions.push(self.parse_expression());
        while self.eat(Punctuator::Comma) {
            expressions.push(self.parse_expression());
        }
        expressions
    }

    /// Parses a `do body while ( condition ) ;` statement (15.8.2).
    fn parse_do_while(&mut self, start: u32) -> Stmt {
        self.bump();
        let body = Box::new(self.parse_statement());
        self.expect_keyword(Keyword::While, "while");
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        let condition = self.parse_expression();
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        Stmt::new(StmtKind::DoWhile { body, condition }, Span::new(start, end))
    }

    /// Parses a `for` statement (15.8.3): an optional initializer, condition, and
    /// iterator list, each clause separated by `;`, then the body.
    fn parse_for(&mut self, start: u32) -> Stmt {
        self.bump();
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        let initializer = self.parse_for_initializer();
        self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        let condition = if self.current_punctuator() == Some(Punctuator::Semicolon) {
            None
        } else {
            Some(self.parse_expression())
        };
        self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        let iterators = if self.current_punctuator() == Some(Punctuator::CloseParen) {
            Vec::new()
        } else {
            self.parse_expression_list()
        };
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        let body = Box::new(self.parse_statement());
        let end = body.span.end;
        Stmt::new(
            StmtKind::For {
                initializer,
                condition,
                iterators,
                body,
            },
            Span::new(start, end),
        )
    }

    /// Parses a `for` initializer (15.8.3): a local declaration (a type then an
    /// identifier) or a list of statement expressions, disambiguated as in a
    /// statement. Returns `None` when the clause is empty.
    fn parse_for_initializer(&mut self) -> Option<ForInitializer> {
        if self.current_punctuator() == Some(Punctuator::Semicolon) {
            return None;
        }
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        let ty = self.parse_type();
        if !matches!(ty.kind, TypeRefKind::Error)
            && matches!(self.current().kind, TokenKind::Identifier(_))
        {
            let declarators = self.parse_variable_declarators(InitializerRefs::ByValueLocal);
            return Some(ForInitializer::Declaration { ty, declarators });
        }
        self.position = saved_position;
        self.diagnostics.truncate(saved_diagnostics);
        Some(ForInitializer::Expressions(self.parse_expression_list()))
    }

    /// Parses a `foreach ( type name in collection ) body` statement (15.8.4).
    fn parse_foreach(&mut self, start: u32) -> Stmt {
        self.bump();
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        if let Some(deconstruction) = self.try_parse_foreach_deconstruction(start) {
            return deconstruction;
        }
        let ty = self.parse_type();
        let (name, _) = self.expect_declared_name();
        if !self.eat_keyword(Keyword::In) {
            let at = self.current().span.start;
            self.report(DiagnosticKind::InExpected, Span::empty_at(at));
        }
        let collection = self.parse_expression();
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        let body = Box::new(self.parse_statement());
        let end = body.span.end;
        Stmt::new(
            StmtKind::ForEach {
                ty,
                name,
                collection,
                body,
            },
            Span::new(start, end),
        )
    }

    /// A `foreach (var (a, b) in e)` / `foreach ((int a, int b) in e)` header (C# 7.0), with the
    /// scanner just past the `(`; `None` when the header declares one ordinary variable.
    ///
    /// Speculated and rewound exactly as [`Parser::try_parse_deconstruction`] is, and for the same
    /// reason -- the target list and a parenthesized type share their tokens -- with `in` in place
    /// of the `=` as the token that settles it.
    fn try_parse_foreach_deconstruction(&mut self, start: u32) -> Option<Stmt> {
        let designated = matches!(self.current_contextual_keyword(), Some("var"))
            && self.next_is(Punctuator::OpenParen);
        if !designated && self.current_punctuator() != Some(Punctuator::OpenParen) {
            return None;
        }
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        let var_span = if designated {
            let at = self.current().span;
            self.bump();
            Some(at)
        } else {
            None
        };
        if !self.deconstruction_ahead(DeconstructionTerminator::In) {
            self.position = saved_position;
            return None;
        }
        let bracket = self.current().span.start;
        if let Some((targets, _)) = self.try_parse_deconstruction_targets(designated)
            && self.current_keyword() == Some(Keyword::In)
        {
            self.gate_feature(Feature::Tuples, Span::empty_at(bracket));
            self.bump();
            let collection = self.parse_expression();
            self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
            let body = Box::new(self.parse_statement());
            let end = body.span.end;
            return Some(Stmt::new(
                StmtKind::ForEachDeconstruction {
                    var_span,
                    targets,
                    collection,
                    body,
                },
                Span::new(start, end),
            ));
        }
        self.position = saved_position;
        self.diagnostics.truncate(saved_diagnostics);
        None
    }

    /// Parses a bare keyword statement terminated by `;`, used for `break` and
    /// `continue` (15.9.1, 15.9.2).
    fn parse_keyword_then_semicolon(&mut self, start: u32, kind: StmtKind) -> Stmt {
        self.bump();
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        Stmt::new(kind, Span::new(start, end))
    }

    /// Parses a `throw expression_opt ;` statement (15.9.5).
    fn parse_throw(&mut self, start: u32) -> Stmt {
        self.bump();
        let value = if self.current_punctuator() == Some(Punctuator::Semicolon) {
            None
        } else {
            Some(self.parse_expression())
        };
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        Stmt::new(StmtKind::Throw(value), Span::new(start, end))
    }

    /// Parses a block where the grammar requires one (a `try`/`catch`/`finally`
    /// body, or a `checked`/`unchecked` block). A missing `{` is `CS1514`,
    /// recovered with an empty block so parsing continues.
    fn parse_required_block(&mut self) -> Stmt {
        if self.current_punctuator() == Some(Punctuator::OpenBrace) {
            self.parse_block()
        } else {
            let at = self.current().span.start;
            self.report(DiagnosticKind::OpenBraceExpected, Span::empty_at(at));
            Stmt::new(StmtKind::Block(Vec::new()), Span::empty_at(at))
        }
    }

    /// Parses a `try` statement (15.10): a protected block, then catch clauses
    /// and/or a finally block.
    fn parse_try(&mut self, start: u32) -> Stmt {
        self.bump();
        let body = Box::new(self.parse_required_block());
        let mut end = body.span.end;
        let mut catches = Vec::new();
        while self.current_keyword() == Some(Keyword::Catch) {
            let clause = self.parse_catch_clause();
            end = clause.body.span.end;
            catches.push(clause);
        }
        let finally_block = if self.eat_keyword(Keyword::Finally) {
            let block = self.parse_required_block();
            end = block.span.end;
            Some(Box::new(block))
        } else {
            None
        };
        if catches.is_empty() && finally_block.is_none() {
            let at = self.current().span.start;
            self.report(DiagnosticKind::ExpectedCatchOrFinally, Span::empty_at(at));
        }
        Stmt::new(
            StmtKind::Try {
                body,
                catches,
                finally_block,
            },
            Span::new(start, end),
        )
    }

    /// Parses one `catch` clause (15.10): an optional `( type name_opt )`, an optional
    /// `when ( condition )` exception filter (C# 6.0), then a block. A bare `catch` is a general
    /// catch, and `catch when (c)` is a general catch WITH a filter -- both halves are optional and
    /// independently so.
    fn parse_catch_clause(&mut self) -> CatchClause {
        let start = self.current().span.start;
        let mut end = self.current().span.end;
        self.bump();
        let (exception_type, name) = if self.eat(Punctuator::OpenParen) {
            let ty = self.parse_type();
            let name = if matches!(self.current().kind, TokenKind::Identifier(_)) {
                Some(self.expect_declared_name().0)
            } else {
                None
            };
            end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
            (Some(ty), name)
        } else {
            (None, None)
        };
        let filter = if matches!(self.current_contextual_keyword(), Some("when")) {
            let at = self.current().span;
            self.bump();
            self.gate_feature(Feature::ExceptionFilter, at);
            self.expect(
                Punctuator::OpenParen,
                DiagnosticKind::TokenExpected { expected: "(" },
            );
            let condition = self.parse_expression();
            end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
            Some(condition)
        } else {
            None
        };
        let body = Box::new(self.parse_required_block());
        CatchClause {
            exception_type,
            name,
            filter,
            body,
            span: Span::new(start, end),
        }
    }

    /// Parses a `lock ( expression ) statement` (15.12).
    fn parse_lock(&mut self, start: u32) -> Stmt {
        self.bump();
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        let expression = self.parse_expression();
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        let body = Box::new(self.parse_statement());
        let end = body.span.end;
        Stmt::new(StmtKind::Lock { expression, body }, Span::new(start, end))
    }

    /// Parses a `fixed ( T* id = expr ) statement` (unsafe, 15.7).
    fn parse_fixed(&mut self, start: u32) -> Stmt {
        self.bump();
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        let ty = self.parse_type();
        let mut declarators: Vec<(Box<str>, Expr)> = Vec::new();
        loop {
            let (name, _) = self.expect_identifier();
            self.expect(Punctuator::Equals, DiagnosticKind::TokenExpected { expected: "=" });
            let init = self.parse_expression();
            declarators.push((name, init));
            if !self.eat(Punctuator::Comma) {
                break;
            }
        }
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        let mut body = Box::new(self.parse_statement());
        let end = body.span.end;
        for (name, init) in declarators.into_iter().rev() {
            body = Box::new(Stmt::new(
                StmtKind::Fixed {
                    ty: ty.clone(),
                    name,
                    init,
                    body,
                },
                Span::new(start, end),
            ));
        }
        *body
    }

    /// Parses a `using ( resource ) statement` (15.13).
    fn parse_using(&mut self, start: u32) -> Stmt {
        if self.using_declaration_here() {
            self.report(
                DiagnosticKind::EmbeddedStatementCannotBeDeclaration,
                Span::empty_at(start),
            );
            self.bump();
            self.gate_feature(Feature::UsingDeclaration, Span::empty_at(start));
            let resource = self.parse_using_resource();
            let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
            let body = Stmt::new(StmtKind::Block(Vec::new()), Span::empty_at(end));
            return Stmt::new(
                StmtKind::Using {
                    resource,
                    body: Box::new(body),
                },
                Span::new(start, end),
            );
        }
        self.bump();
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        let resource = self.parse_using_resource();
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        let body = Box::new(self.parse_statement());
        let end = body.span.end;
        Stmt::new(StmtKind::Using { resource, body }, Span::new(start, end))
    }

    /// Parses a `using` resource (15.13): a local declaration or an expression,
    /// disambiguated as in a statement.
    fn parse_using_resource(&mut self) -> UsingResource {
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        let ty = self.parse_type();
        if !matches!(ty.kind, TypeRefKind::Error)
            && matches!(self.current().kind, TokenKind::Identifier(_))
        {
            let declarators = self.parse_variable_declarators(InitializerRefs::Forbidden);
            return UsingResource::Declaration { ty, declarators };
        }
        self.position = saved_position;
        self.diagnostics.truncate(saved_diagnostics);
        UsingResource::Expression(self.parse_expression())
    }

    /// Parses a `checked`/`unchecked` block statement (15.11), with the scanner
    /// at the keyword and a block known to follow.
    fn parse_checked_block(&mut self, start: u32, keyword: Keyword) -> Stmt {
        self.bump();
        let block = Box::new(self.parse_required_block());
        let end = block.span.end;
        let kind = if keyword == Keyword::Checked {
            StmtKind::Checked(block)
        } else {
            StmtKind::Unchecked(block)
        };
        Stmt::new(kind, Span::new(start, end))
    }

    /// Parses a `switch` statement (15.7.2): `switch ( expression ) { sections }`,
    /// each section a run of `case`/`default` labels followed by statements.
    fn parse_switch(&mut self, start: u32) -> Stmt {
        self.bump();
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        let expression = self.parse_expression();
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        self.expect(Punctuator::OpenBrace, DiagnosticKind::OpenBraceExpected);
        let mut sections = Vec::new();
        while self.current_punctuator() != Some(Punctuator::CloseBrace)
            && !matches!(self.current().kind, TokenKind::EndOfFile)
        {
            let before = self.position;
            let mut labels = Vec::new();
            while let Some(label) = self.try_parse_switch_label() {
                labels.push(label);
            }
            let mut statements = Vec::new();
            while !self.at_switch_label()
                && self.current_punctuator() != Some(Punctuator::CloseBrace)
                && !matches!(self.current().kind, TokenKind::EndOfFile)
            {
                let statement_start = self.position;
                statements.push(self.parse_statement());
                if self.position == statement_start {
                    self.bump();
                }
            }
            sections.push(SwitchSection { labels, statements });
            if self.position == before {
                self.bump();
            }
        }
        let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
        Stmt::new(
            StmtKind::Switch {
                expression,
                sections,
            },
            Span::new(start, end),
        )
    }

    /// Parses a `case constant-expression :` or `default :` label, if one begins
    /// here (15.7.2).
    fn try_parse_switch_label(&mut self) -> Option<SwitchLabel> {
        match self.current_keyword() {
            Some(Keyword::Case) => {
                self.bump();
                let value = self.parse_expression();
                self.expect(
                    Punctuator::Colon,
                    DiagnosticKind::TokenExpected { expected: ":" },
                );
                Some(SwitchLabel::Case(value))
            }
            Some(Keyword::Default) if self.at_switch_label() => {
                self.bump();
                self.expect(
                    Punctuator::Colon,
                    DiagnosticKind::TokenExpected { expected: ":" },
                );
                Some(SwitchLabel::Default)
            }
            _ => None,
        }
    }

    /// Whether the scanner is at the start of a switch LABEL -- `case`, or a `default` that a `:`
    /// follows.
    ///
    /// **THE `default` HALF NEEDS THE LOOKAHEAD, AND BOTH LOOPS MUST AGREE ON IT.** A section's
    /// label run and its statement run are separate loops over the same tokens: one collects while
    /// a label starts here, the other collects until one does. `default` begins a label AND the
    /// `default(T)` operator, so a definition that stops at the keyword makes
    /// `case 1: default(int).ToString();` -- a legal program -- terminate the statement run at a
    /// label that is not there. The section loop's no-progress guard then BUMPS PAST the keyword,
    /// so the failure is a silently dropped token rather than a diagnostic.
    ///
    /// One predicate, both callers, for that reason: two spellings of "a label starts here" is the
    /// shape where one of them gains a case and the other does not.
    fn at_switch_label(&self) -> bool {
        match self.current_keyword() {
            Some(Keyword::Case) => true,
            Some(Keyword::Default) => matches!(
                self.tokens.get(self.position + 1).map(|token| &token.kind),
                Some(TokenKind::Punctuator(Punctuator::Colon))
            ),
            _ => false,
        }
    }

    /// Parses a labeled statement `label : statement` (15.4), with the scanner at
    /// the identifier.
    fn parse_labeled(&mut self, start: u32) -> Stmt {
        let (label, _) = self.expect_identifier();
        self.expect(
            Punctuator::Colon,
            DiagnosticKind::TokenExpected { expected: ":" },
        );
        let statement = Box::new(self.parse_statement());
        let end = statement.span.end;
        Stmt::new(
            StmtKind::Labeled { label, statement },
            Span::new(start, end),
        )
    }

    /// Parses a `goto` statement (15.9.3): `goto label ;`, `goto case e ;`, or
    /// `goto default ;`.
    fn parse_goto(&mut self, start: u32) -> Stmt {
        self.bump();
        let target = match self.current_keyword() {
            Some(Keyword::Case) => {
                self.bump();
                GotoTarget::Case(self.parse_expression())
            }
            Some(Keyword::Default) => {
                self.bump();
                GotoTarget::Default
            }
            _ => GotoTarget::Label(self.expect_identifier().0),
        };
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        Stmt::new(StmtKind::Goto(target), Span::new(start, end))
    }

    /// Parses a whole compilation unit (16.1): using directives then top-level
    /// namespace and type declarations, to end of file.
    fn parse_compilation_unit(&mut self) -> CompilationUnit {
        let start = self.current().span.start;
        let usings = self.parse_using_directives();
        let mut members = Vec::new();
        let mut global_attributes = Vec::new();
        while !matches!(self.current().kind, TokenKind::EndOfFile) {
            let before = self.position;
            if self.current_punctuator() == Some(Punctuator::OpenBracket)
                && self.is_global_attribute_target()
            {
                if !members.is_empty() {
                    let target = self.tokens.get(self.position + 1).map(|token| token.span);
                    let span = target.unwrap_or_else(|| self.current().span);
                    self.report(DiagnosticKind::GlobalAttributeMustPrecedeMembers, span);
                }
                global_attributes.push(self.parse_attribute_section());
                continue;
            }
            members.push(self.parse_namespace_member(NamespaceContainer::CompilationUnit {
                members_precede: !members.is_empty(),
            }));
            if self.position == before {
                self.bump();
            }
        }
        let end = self.current().span.start;
        CompilationUnit {
            usings,
            members,
            global_attributes,
            span: Span::new(start, end),
            defined_symbols: core::mem::take(&mut self.defined_symbols),
        }
    }

    /// Whether the attribute section starting at the current `[` targets `assembly` or `module`
    /// (24.2) -- a global attribute attaching to the assembly/module manifest, not a following
    /// declaration. A 2-token lookahead (`[ assembly|module :`), so a member's `[Attr]` is left
    /// for the normal member-attribute path.
    fn is_global_attribute_target(&self) -> bool {
        let target = self
            .tokens
            .get(self.position + 1)
            .and_then(|token| match &token.kind {
                TokenKind::Identifier(text) => Some(&**text),
                TokenKind::Keyword(keyword) => Some(keyword.as_str()),
                _ => None,
            });
        matches!(target, Some("assembly") | Some("module"))
            && matches!(
                self.tokens.get(self.position + 2).map(|token| &token.kind),
                Some(TokenKind::Punctuator(Punctuator::Colon))
            )
    }

    /// Parses a run of leading `using` directives (16.3).
    fn parse_using_directives(&mut self) -> Vec<UsingDirective> {
        let mut directives = Vec::new();
        while self.current_keyword() == Some(Keyword::Using) {
            directives.push(self.parse_using_directive());
        }
        directives
    }

    /// Parses one `using` directive (16.3): a namespace import, a static-type import, or an alias.
    fn parse_using_directive(&mut self) -> UsingDirective {
        let start = self.current().span.start;
        self.bump();
        let is_static = self.current_keyword() == Some(Keyword::Static);
        if is_static {
            self.gate_feature_here(Feature::UsingStatic);
            self.bump();
        }
        let kind = if is_static {
            UsingKind::Static(self.parse_qualified_name())
        } else if matches!(self.current().kind, TokenKind::Identifier(_))
            && self.next_is(Punctuator::Equals)
        {
            let (name, _) = self.expect_identifier();
            self.bump();
            UsingKind::Alias {
                name,
                target: self.parse_qualified_name(),
            }
        } else {
            UsingKind::Namespace(self.parse_qualified_name())
        };
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        UsingDirective {
            kind,
            span: Span::new(start, end),
        }
    }

    /// Parses a dotted name `a.b.c` (10.8).
    fn parse_qualified_name(&mut self) -> QualifiedName {
        let start = self.current().span.start;
        let mut parts = Vec::new();
        let (first, mut end) = self.expect_identifier();
        parts.push(first);
        while self.current_punctuator() == Some(Punctuator::Dot) {
            self.bump();
            let (part, part_end) = self.expect_identifier();
            end = part_end;
            parts.push(part);
        }
        QualifiedName {
            parts,
            span: Span::new(start, end),
        }
    }

    /// Parses zero or more leading attribute sections `[ ... ]` (clause 24).
    fn parse_attribute_sections(&mut self) -> Vec<AttributeSection> {
        let mut sections = Vec::new();
        while self.current_punctuator() == Some(Punctuator::OpenBracket) {
            sections.push(self.parse_attribute_section());
        }
        sections
    }

    /// Parses one attribute section `[ target? attribute-list ]` (24.1), the
    /// scanner at the `[`. A trailing comma in the list is allowed.
    fn parse_attribute_section(&mut self) -> AttributeSection {
        let start = self.current().span.start;
        self.bump();
        let target = if self.next_is(Punctuator::Colon)
            && matches!(
                self.current().kind,
                TokenKind::Identifier(_) | TokenKind::Keyword(_)
            ) {
            let target: Box<str> = match &self.current().kind {
                TokenKind::Keyword(keyword) => keyword.as_str().into(),
                TokenKind::Identifier(text) => text.clone(),
                _ => unreachable!(),
            };
            self.bump();
            self.bump();
            Some(target)
        } else {
            None
        };
        let mut attributes = Vec::new();
        attributes.push(self.parse_attribute());
        while self.eat(Punctuator::Comma) {
            if self.current_punctuator() == Some(Punctuator::CloseBracket) {
                break;
            }
            attributes.push(self.parse_attribute());
        }
        let end = self.expect(
            Punctuator::CloseBracket,
            DiagnosticKind::TokenExpected { expected: "]" },
        );
        AttributeSection {
            target,
            attributes,
            span: Span::new(start, end),
        }
    }

    /// Parses one attribute: a type name and an optional argument list (24.2).
    fn parse_attribute(&mut self) -> Attribute {
        let start = self.current().span.start;
        let name = self.parse_qualified_name();
        let mut end = name.span.end;
        let arguments = if self.current_punctuator() == Some(Punctuator::OpenParen) {
            let (arguments, close) = self.parse_attribute_arguments();
            end = close;
            arguments
        } else {
            Vec::new()
        };
        Attribute {
            name,
            arguments,
            span: Span::new(start, end),
        }
    }

    /// Parses an attribute's parenthesized argument list (24.2): positional
    /// arguments then named `name = value` arguments. Returns the arguments and
    /// the offset past the closing `)`.
    fn parse_attribute_arguments(&mut self) -> (Vec<AttributeArgument>, u32) {
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        let mut arguments = Vec::new();
        if self.current_punctuator() != Some(Punctuator::CloseParen) {
            loop {
                if matches!(self.current().kind, TokenKind::Identifier(_))
                    && self.next_is(Punctuator::Equals)
                {
                    let (name, _) = self.expect_identifier();
                    self.bump();
                    let value = self.parse_expression();
                    arguments.push(AttributeArgument::Named { name, value });
                } else {
                    arguments.push(AttributeArgument::Positional(self.parse_expression()));
                }
                if !self.eat(Punctuator::Comma) {
                    break;
                }
            }
        }
        let end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        (arguments, end)
    }

    /// Parses a namespace member (16.4): a nested namespace or a type declaration,
    /// with any leading attribute sections.
    ///
    /// `container` says what this member is being declared inside, which is the entire input to
    /// the file-scoped-namespace placement rules -- see [`NamespaceContainer`].
    fn parse_namespace_member(&mut self, container: NamespaceContainer) -> NamespaceMember {
        let start = self.current().span.start;
        let attributes = self.parse_attribute_sections();
        let modifiers_from = self.position;
        let modifiers = self.parse_modifiers();
        let modifiers_to = self.position;
        if self.current_keyword() == Some(Keyword::Namespace) {
            if matches!(container, NamespaceContainer::CompilationUnit { .. }) {
                let modifier_spans: Vec<Span> = (modifiers_from..modifiers_to)
                    .filter_map(|index| self.tokens.get(index).map(|token| token.span))
                    .collect();
                for section in &attributes {
                    self.report(
                        DiagnosticKind::NamespaceCannotHaveModifiersOrAttributes,
                        section.span,
                    );
                }
                for span in modifier_spans {
                    self.report(DiagnosticKind::NamespaceCannotHaveModifiersOrAttributes, span);
                }
            }
            return NamespaceMember::Namespace(self.parse_namespace_declaration(container));
        }
        self.parse_type_kind_declaration(attributes, modifiers, start)
    }

    /// Parses a type declaration given its already-parsed attributes and modifiers
    /// (16.5): a class, struct, interface, enum, or delegate.
    fn parse_type_kind_declaration(
        &mut self,
        attributes: Vec<AttributeSection>,
        modifiers: Vec<Modifier>,
        start: u32,
    ) -> NamespaceMember {
        if self.record_declaration_here() {
            return NamespaceMember::Type(self.parse_record(attributes, modifiers, start));
        }
        match self.current_keyword() {
            Some(Keyword::Enum) => {
                NamespaceMember::Enum(self.parse_enum(attributes, modifiers, start))
            }
            Some(Keyword::Delegate) => {
                NamespaceMember::Delegate(self.parse_delegate(attributes, modifiers, start))
            }
            _ => NamespaceMember::Type(
                self.parse_class_struct_interface(attributes, modifiers, start),
            ),
        }
    }

    /// Parses a `record` declaration (C# 9) -- the same shape as a class, plus an optional
    /// positional parameter list, an optional argument list on its base, and a `;` body.
    ///
    /// It shares [`Parser::parse_class_struct_interface`] rather than copying it, because a record
    /// IS a class declaration in every respect the two have in common: modifiers, type parameters,
    /// a base list, constraint clauses and members. A separate parser would be a second
    /// implementation of all of that, and the next member kind would be added to one of them.
    fn parse_record(
        &mut self,
        attributes: Vec<AttributeSection>,
        modifiers: Vec<Modifier>,
        start: u32,
    ) -> TypeDecl {
        let keyword_span = self.current().span;
        self.bump();
        self.gate_feature(Feature::Records, keyword_span);
        if matches!(
            self.current_keyword(),
            Some(Keyword::Class) | Some(Keyword::Struct)
        ) {
            let end = self.current().span.end;
            self.gate_feature(
                Feature::RecordStructs,
                Span::new(keyword_span.start, end),
            );
        }
        let mut declaration =
            self.parse_class_struct_interface_inner(attributes, modifiers, start, Some(keyword_span));
        if !declaration.bases.is_empty() {
            self.gate_feature(Feature::RecordInheritance, keyword_span);
        }
        synthesize_record_members(&mut declaration);
        declaration
    }

    /// Consumes the OPTIONAL semicolon that may follow a type or namespace declaration's closing
    /// brace, and reports nothing when there is none.
    ///
    /// **IT IS IN THE GRAMMAR OF EVERY ONE OF THEM, NOT A COURTESY** -- ECMA-334 spells
    /// `class-body ;opt`, `struct-body ;opt`, `interface-body ;opt`, `enum-body ;opt` and
    /// `namespace-body ;opt`, and has since the first edition. It is the C-and-C++ habit written
    /// into C# 1.0 so that source carried over from either still compiles, which is exactly where
    /// it turns up in practice.
    ///
    /// **A `;` HERE IS SILENT AND A `;;` IS NOT.** Only one is in the production, so a second still
    /// reaches the enclosing member or namespace loop and is still a stray token there.
    ///
    ///
    fn eat_declaration_semicolon(&mut self) {
        self.eat(Punctuator::Semicolon);
    }

    /// Parses an `enum` declaration (21): the kind keyword, a name, an optional
    /// `: integral-type` base, then comma-separated members allowing a trailing
    /// comma.
    fn parse_enum(
        &mut self,
        attributes: Vec<AttributeSection>,
        modifiers: Vec<Modifier>,
        start: u32,
    ) -> EnumDecl {
        self.bump();
        let (name, _) = self.expect_identifier();
        let base = if self.eat(Punctuator::Colon) {
            Some(self.parse_type())
        } else {
            None
        };
        self.expect(Punctuator::OpenBrace, DiagnosticKind::OpenBraceExpected);
        let mut members = Vec::new();
        loop {
            if self.current_punctuator() == Some(Punctuator::CloseBrace)
                || matches!(self.current().kind, TokenKind::EndOfFile)
                || !(matches!(self.current().kind, TokenKind::Identifier(_))
                    || self.current_punctuator() == Some(Punctuator::OpenBracket))
            {
                break;
            }
            let member_start = self.current().span.start;
            let member_attributes = self.parse_attribute_sections();
            let (member_name, mut member_end) = self.expect_identifier();
            let value = if self.eat(Punctuator::Equals) {
                let value = self.parse_expression();
                member_end = value.span.end;
                Some(value)
            } else {
                None
            };
            members.push(EnumMember {
                attributes: member_attributes,
                name: member_name,
                value,
                span: Span::new(member_start, member_end),
            });
            if !self.eat(Punctuator::Comma) {
                break;
            }
        }
        let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
        self.eat_declaration_semicolon();
        EnumDecl {
            attributes,
            modifiers,
            name,
            base,
            members,
            span: Span::new(start, end),
        }
    }

    /// Parses a `delegate` declaration (22):
    /// `delegate return-type name type-parameter-list? ( params ) constraints? ;`.
    ///
    /// **THE TYPE-PARAMETER LIST AND THE CONSTRAINT CLAUSES SIT ON EITHER SIDE OF THE PARAMETER
    /// LIST, WHICH IS THE METHOD SHAPE AND NOT THE TYPE SHAPE** -- ECMA-334 4th ed 22.1 spells
    /// `delegate-declaration: ... identifier type-parameter-list_opt ( formal-parameter-list_opt )
    /// type-parameter-constraints-clauses_opt ;`. A class puts its clauses after the BASE list, and
    /// a delegate has no base list to put them after; the parameter list is what separates them
    /// here. Both halves come from the shared parsers, so a delegate gates and diagnoses exactly as
    /// a generic method does.
    ///
    /// `delegate void D<T>()` and `delegate void D()` are DIFFERENT TYPES that may be declared side
    /// by side, which is why the arity has to reach the model and the metadata name rather than
    /// stopping at the parser: see [`crate::ast::DelegateDecl::type_parameters`].
    fn parse_delegate(
        &mut self,
        attributes: Vec<AttributeSection>,
        modifiers: Vec<Modifier>,
        start: u32,
    ) -> DelegateDecl {
        self.bump();
        let return_type = self.parse_type();
        let (name, _) = self.expect_identifier();
        let type_parameters = self.parse_type_parameter_list();
        let (parameters, arglist) = self.parse_parameter_list();
        if let Some(span) = arglist {
            self.report(DiagnosticKind::ArglistNotValidInThisContext, span);
        }
        let constraints = self.parse_type_parameter_constraint_clauses();
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        DelegateDecl {
            attributes,
            modifiers,
            return_type,
            name,
            type_parameters,
            constraints,
            parameters,
            span: Span::new(start, end),
        }
    }

    /// Parses a `namespace` declaration (16.2), in either the brace-delimited form or the
    /// file-scoped form `namespace N;` (C# 10).
    ///
    /// The two forms are told apart by the token after the name -- `{` or `;` -- and produce the
    /// same [`NamespaceDecl`] afterwards, because they declare the same namespace. All that
    /// differs is where the body ends: a brace for one, the end of the enclosing container for the
    /// other.
    fn parse_namespace_declaration(&mut self, container: NamespaceContainer) -> NamespaceDecl {
        let keyword = self.current().span;
        let start = keyword.start;
        self.bump();
        let diagnostics_before = self.diagnostics.len();
        let name = self.parse_qualified_name();
        let named = self.diagnostics.len() == diagnostics_before;
        if self.current_punctuator() == Some(Punctuator::Semicolon) {
            return self.parse_file_scoped_namespace_body(container, keyword, name, start, named);
        }
        if named && container == NamespaceContainer::FileScoped {
            self.report(
                DiagnosticKind::BothFileScopedAndNormalNamespaces,
                name.span,
            );
        }
        self.expect(Punctuator::OpenBrace, DiagnosticKind::OpenBraceExpected);
        let usings = self.parse_using_directives();
        let mut members = Vec::new();
        while self.current_punctuator() != Some(Punctuator::CloseBrace)
            && !matches!(self.current().kind, TokenKind::EndOfFile)
        {
            let before = self.position;
            members.push(self.parse_namespace_member(NamespaceContainer::Block));
            if self.position == before {
                self.bump();
            }
        }
        let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
        self.eat_declaration_semicolon();
        NamespaceDecl {
            name,
            usings,
            members,
            file_scoped: false,
            span: Span::new(start, end),
        }
    }

    /// Parses the rest of a file-scoped namespace declaration (C# 10), positioned on its `;`.
    ///
    /// **Its body is everything up to the end of its container**, so there is no closing brace to
    /// expect and no second list to build: the members it collects are exactly the ones the
    /// enclosing loop would otherwise have collected itself. That is what makes this feature pure
    /// syntax -- the tree it produces is the tree the brace-delimited spelling produces, so the
    /// binder, the metadata writer and the emitter see nothing new.
    ///
    /// Stopping at a `}` as well as at end-of-file is not defensive: it is what keeps a
    /// MISPLACED file-scoped namespace (one written inside `namespace M { ... }`) from swallowing
    /// the enclosing declaration's brace and turning one reportable mistake into a parse cascade.
    ///
    /// The three placement rules are csc's, MEASURED one compilation each, and every
    /// one of them turns on the immediate CONTAINER rather than on anything file-wide:
    ///
    /// | container | diagnostic |
    /// |---|---|
    /// | the compilation unit, nothing declared yet | none -- this is the form the feature is for |
    /// | the compilation unit, a type or namespace already declared | `CS8956` |
    /// | a brace-delimited namespace | `CS8955` |
    /// | another file-scoped namespace | `CS8954` |
    ///
    /// **`CS8954`'s message says "only one ... per file" and the rule is not that.** Because a
    /// file-scoped body runs to end of file, a second file-scoped declaration is never a sibling of
    /// the first -- it is always nested inside it, which is why the container answers this too.
    ///
    /// The version gate is reported separately and does not replace any of the three: csc emits
    /// both, at every dialect, and so do we.
    fn parse_file_scoped_namespace_body(
        &mut self,
        container: NamespaceContainer,
        keyword: Span,
        name: QualifiedName,
        start: u32,
        named: bool,
    ) -> NamespaceDecl {
        if named {
            self.gate_feature(Feature::FileScopedNamespaces, keyword);
        }
        match container {
            _ if !named => {}
            NamespaceContainer::CompilationUnit { members_precede: true } => self.report(
                DiagnosticKind::FileScopedNamespaceMustPrecedeMembers,
                name.span,
            ),
            NamespaceContainer::Block => {
                self.report(DiagnosticKind::BothFileScopedAndNormalNamespaces, name.span);
            }
            NamespaceContainer::FileScoped => {
                self.report(DiagnosticKind::OnlyOneFileScopedNamespace, name.span);
            }
            NamespaceContainer::CompilationUnit { members_precede: false } => {}
        }
        self.bump();
        let usings = self.parse_using_directives();
        let mut members = Vec::new();
        while self.current_punctuator() != Some(Punctuator::CloseBrace)
            && !matches!(self.current().kind, TokenKind::EndOfFile)
        {
            let before = self.position;
            members.push(self.parse_namespace_member(NamespaceContainer::FileScoped));
            if self.position == before {
                self.bump();
            }
        }
        NamespaceDecl {
            name,
            usings,
            members,
            file_scoped: true,
            span: Span::new(start, self.current().span.start),
        }
    }

    /// Parses a class, struct, or interface declaration given its already-parsed
    /// attributes and modifiers (17, 18, 20): the kind keyword, a name, an
    /// optional base list, and a member body.
    fn parse_class_struct_interface(
        &mut self,
        attributes: Vec<AttributeSection>,
        modifiers: Vec<Modifier>,
        start: u32,
    ) -> TypeDecl {
        self.parse_class_struct_interface_inner(attributes, modifiers, start, None)
    }

    /// The body of [`Parser::parse_class_struct_interface`], shared with [`Parser::parse_record`].
    ///
    /// `record_keyword` is `Some(span)` when the contextual `record` has already been consumed,
    /// and it changes exactly four things -- the kind keyword becomes OPTIONAL (`record R` is a
    /// class), a positional parameter list may follow the name, the FIRST base may carry an
    /// argument list, and `;` is a legal body. Everything else is the same grammar, which is why
    /// this is one function: a copy would be a second place to add the next member kind to.
    fn parse_class_struct_interface_inner(
        &mut self,
        attributes: Vec<AttributeSection>,
        modifiers: Vec<Modifier>,
        start: u32,
        record_keyword: Option<Span>,
    ) -> TypeDecl {
        let mut keyword_form = false;
        let kind = match self.current_keyword() {
            Some(Keyword::Class) => {
                self.bump();
                keyword_form = true;
                TypeKind::Class
            }
            Some(Keyword::Struct) => {
                self.bump();
                keyword_form = true;
                TypeKind::Struct
            }
            Some(Keyword::Interface) => {
                self.bump();
                TypeKind::Interface
            }
            _ if record_keyword.is_some() => TypeKind::Class,
            _ => {
                let at = self.current().span.start;
                self.report(DiagnosticKind::TypeDeclarationExpected, Span::empty_at(at));
                TypeKind::Class
            }
        };
        let (name, _) = self.expect_identifier();
        let type_parameters = self.parse_type_parameter_list();
        let parameters = if record_keyword.is_some()
            && self.current_punctuator() == Some(Punctuator::OpenParen)
        {
            Some(self.parse_parameter_list().0)
        } else {
            None
        };
        let mut base_arguments = None;
        let bases = if self.eat(Punctuator::Colon) {
            let mut bases = Vec::new();
            bases.push(self.parse_type());
            if record_keyword.is_some() && self.current_punctuator() == Some(Punctuator::OpenParen)
            {
                self.bump();
                let (arguments, _) = self
                    .parse_arguments(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
                base_arguments = Some(arguments);
            }
            while self.eat(Punctuator::Comma) {
                bases.push(self.parse_type());
            }
            bases
        } else {
            Vec::new()
        };
        let constraints = self.parse_type_parameter_constraint_clauses();
        let mut members = Vec::new();
        let end = if record_keyword.is_some()
            && self.current_punctuator() == Some(Punctuator::Semicolon)
        {
            let end = self.current().span.end;
            self.bump();
            end
        } else {
            self.expect(Punctuator::OpenBrace, DiagnosticKind::OpenBraceExpected);
            while self.current_punctuator() != Some(Punctuator::CloseBrace)
                && !matches!(self.current().kind, TokenKind::EndOfFile)
            {
                let before = self.position;
                members.push(self.parse_member());
                if self.position == before {
                    self.bump();
                }
            }
            let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
            self.eat_declaration_semicolon();
            end
        };
        TypeDecl {
            attributes,
            modifiers,
            kind,
            name,
            type_parameters,
            bases,
            constraints,
            members,
            record: record_keyword.map(|keyword_span| RecordParts {
                parameters,
                base_arguments,
                keyword_form,
                keyword_span,
            }),
            span: Span::new(start, end),
        }
    }

    /// Whether the identifier `required` at the current position is the C# 11 MODIFIER rather than
    /// an ordinary identifier naming the member's own type.
    ///
    /// **MEASURED against csc, and the rule is version-dependent in a way no amount of reading the
    /// grammar would suggest:**
    ///
    /// | source | below C# 11 | C# 11 and up |
    /// |---|---|---|
    /// | `required int f;` | modifier (and gated) | modifier |
    /// | `required Foo f;` | modifier (and gated) | modifier |
    /// | `required f;` | **a field `f` of type `required`** | modifier, then `CS1519` |
    ///
    /// **csc does this lookahead only BELOW C# 11, to keep source that names a type `required`
    /// compiling; at C# 11 it stops and the identifier becomes a real modifier unconditionally.**
    /// That is a deliberate source-compatibility break in csc -- `class required { }` used as a
    /// field type compiles at C# 10 and does not at C# 11 -- and matching it means accepting it.
    ///
    /// Recognizing it below C# 11 is what buys the clean `Feature 'required members' ... please use
    /// language version 11.0` diagnostic instead of a parse cascade, which is the whole point of
    /// having the gate.
    fn required_is_a_modifier_here(&self) -> bool {
        if !matches!(self.current_contextual_keyword(), Some("required")) {
            return false;
        }
        if self.version >= LanguageVersion::CSharp11 {
            return true;
        }
        let starts_a_type = matches!(
            self.tokens.get(self.position + 1).map(|token| &token.kind),
            Some(TokenKind::Identifier(_)) | Some(TokenKind::Keyword(_))
        );
        let then_a_name = matches!(
            self.tokens.get(self.position + 2).map(|token| &token.kind),
            Some(TokenKind::Identifier(_))
        );
        starts_a_type && then_a_name
    }

    /// Whether the current `async` identifier is the C# 5 MODIFIER rather than a type or an
    /// ordinary name (ECMA-334 5th ed, 15.15). All rows measured against csc, one compilation each:
    ///
    /// | source | csc reads it as |
    /// |---|---|
    /// | `async Task M() { }` | modifier |
    /// | `async async async() { }` | modifier, then type `async`, then name |
    /// | `async async() { ... }` | **a method named `async` returning type `async`** |
    /// | `async f;` | **a field `f` of type `async`** |
    /// | `async C() { }` | **type `async`, method `C`** (CS0246 + CS0542, not a ctor) |
    /// | `async int f;` | modifier, then CS0106 from the binder |
    ///
    /// So it is a modifier exactly when what follows can only be read as the REST of a member
    /// header: another modifier, a predefined type (including `void`), or a scannable type
    /// followed by an identifier. Unlike [`Parser::required_is_a_modifier_here`]'s two-token
    /// peek, the last case must SCAN A FULL TYPE speculatively -- `async Task<int> M()` puts
    /// arbitrarily many tokens between the modifier and the name that proves it is one.
    ///
    /// `@async` is never a modifier: the verbatim prefix forces the ordinary identifier (9.4.2).
    /// Whether the identifier `partial` at the current position is the C# 2.0 MODIFIER rather than
    /// an ordinary identifier (ECMA-334 4th ed 17.1.4).
    ///
    /// **ONE TOKEN OF LOOKAHEAD IS THE WHOLE RULE, AND THAT IS THE STANDARD'S DOING RATHER THAN A
    /// SHORTCUT.** 17.1.4 admits `partial` only IMMEDIATELY before the declaration's keyword, so
    /// unlike [`Parser::required_is_a_modifier_here`]'s two-token peek and
    /// [`Parser::async_is_a_modifier_here`]'s speculative type scan, there is nothing to
    /// disambiguate: what follows is `class`, `struct` or `interface`, or this is not a modifier.
    ///
    /// Measured against csc, and both rows matter: `public class partial { }` declares a TYPE
    /// named `partial`, and `class C { partial x; }` declares a FIELD of that type. Neither
    /// compiles if the identifier is claimed unconditionally.
    ///
    /// `@partial` is never a modifier: the verbatim prefix forces the ordinary identifier (9.4.2),
    /// which [`Parser::current_contextual_keyword`] already enforces for every contextual keyword.
    /// Whether a RECORD declaration begins at the current position (C# 9): the contextual keyword
    /// `record`, then an identifier or the `class`/`struct` keyword.
    ///
    /// **THE TEST IS UNCONDITIONAL, AND THAT IS MEASURED RATHER THAN ASSUMED -- IT IS NOT HOW THE
    /// OTHER CONTEXTUAL KEYWORDS IN THIS FILE BEHAVE.** `partial`, `async` and `required` each
    /// need a lookahead because the identifier may be naming a type; `record` does not, because
    /// csc takes the declaration whatever else is in scope. Three rows measured, one compilation
    /// each, with a `class record { }` declared beside them:
    ///
    /// | source | csc |
    /// |---|---|
    /// | `class C { record x; }` | a nested private RECORD named `x`, plus `CS8860` on the class |
    /// | `class C { record x = null; }` | `CS1514 { expected` -- a record, then a stray `=` |
    /// | `class C { record M() { return null; } }` | `CS1519` -- a POSITIONAL record `M`, then `return` |
    ///
    /// The second and third rows are the ones that settle it: both are unambiguous field and
    /// method shapes, both are stolen by the record grammar anyway. A parser that disambiguated
    /// against a field would accept two programs csc rejects.
    ///
    /// `@record` is never this: [`Parser::current_contextual_keyword`] answers `None` for a
    /// verbatim identifier, and csc agrees -- `public @record R(int X);` is `CS0106`, a METHOD `R`
    /// returning a type called `record`.
    fn record_declaration_here(&self) -> bool {
        matches!(self.current_contextual_keyword(), Some("record"))
            && matches!(
                self.tokens.get(self.position + 1).map(|token| &token.kind),
                Some(TokenKind::Identifier(_))
                    | Some(TokenKind::Keyword(Keyword::Class | Keyword::Struct))
            )
    }

    /// Whether the `ref` at the current position is a STRUCT MODIFIER (C# 7.2) rather than the
    /// `ref` of a ref-returning member, a ref local, or a `ref` parameter.
    ///
    /// **ONLY `ref struct` AND `ref partial struct`**, measured against csc: `ref class` and
    /// `ref interface` are CS1031 there, so csc does not take `ref` as a modifier before them
    /// either, and `partial ref struct` is CS1585 -- the order is fixed and `ref` comes first.
    ///
    /// The lookahead may not be widened to "a modifier run ending in `struct`" the way
    /// [`Parser::partial_is_misplaced_here`] scans: `ref readonly int M()` starts with exactly
    /// that shape and is an ordinary ref-returning method.
    /// The type of a member declaration, which is the ONE position in the grammar where a `ref`
    /// return type may be written: `ref T M()`, `ref T this[int i]`, `ref T P { get; }` (C# 7.0)
    /// and `ref readonly T` at each (C# 7.2).
    ///
    /// **THERE ARE EXACTLY TWO PRODUCERS OF [`TypeRefKind::ByRef`] -- THIS AND
    /// [`Parser::parse_ref_local_type`] -- AND THAT IS WHAT KEEPS `ref` OUT OF THE POSITIONS THAT
    /// MAY NOT HAVE IT.** A field, a parameter's type, a type argument and an array element are all
    /// read by `parse_type`, which has no `ref` arm at all -- so none of them can produce one
    /// however the source is spelled, and no later phase needs to refuse what it cannot be handed.
    /// The alternative, admitting `ref` inside `parse_type` and refusing it afterwards at every
    /// site that must not have it, is the shape [`a rule with several implementations`] takes: a
    /// list of sites, one of which is forgotten.
    ///
    ///
    /// A `ref` here is not the `ref struct` MODIFIER, which [`Parser::parse_modifiers`] has
    /// already consumed when it applies: `ref_is_a_modifier_here` looks for `struct` or
    /// `partial struct` after it, and everything else is this.
    ///
    /// [`a rule with several implementations`]: Parser::parse_member_type
    fn parse_member_type(&mut self) -> TypeRef {
        if self.current_keyword() != Some(Keyword::Ref) {
            return self.parse_type();
        }
        self.parse_by_ref_type()
    }

    /// Parses `ref T` / `ref readonly T` (C# 7.0 / 7.2) at a position that admits one, with the
    /// current token already known to be `ref`.
    ///
    /// **THE TWO PRODUCERS SHARE THIS BODY SO THEY CANNOT GATE DIFFERENTLY.** Both gates, the
    /// `ref void` refusal and the span arithmetic were written once for the member position; a
    /// ref LOCAL needs every one of them at the same rungs, and a second copy is the shape
    /// [`a rule with several implementations`] takes -- three qualifiers, none of which gained the
    /// new case.
    ///
    /// The two gates fire at their OWN tokens, four columns apart, because that is where csc puts
    /// them: measured at `/langversion:6`, `ref readonly int M()` draws CS8059 twice, *byref locals
    /// and returns* at the `ref` and *readonly references* at the `readonly`.
    ///
    /// [`a rule with several implementations`]: Parser::parse_member_type
    fn parse_by_ref_type(&mut self) -> TypeRef {
        let start = self.current().span;
        self.bump();
        self.gate_feature(Feature::ByRefLocalsAndReturns, start);
        let is_readonly = if self.current_keyword() == Some(Keyword::Readonly) {
            let at = self.current().span;
            self.bump();
            self.gate_feature(Feature::ReadOnlyReferences, at);
            true
        } else {
            false
        };
        let referent = self.parse_type();
        if Self::type_is_void(&referent) {
            self.report(DiagnosticKind::VoidByReference, referent.span);
        }
        let end = referent.span.end;
        TypeRef::new(
            TypeRefKind::ByRef {
                referent: Box::new(referent),
                is_readonly,
            },
            Span::new(start.start, end),
        )
    }

    /// The type of a BY-REFERENCE LOCAL DECLARATION: `ref T r = ref e;` and `ref readonly T r`
    /// (C# 7.0 / 7.2). The second producer of [`TypeRefKind::ByRef`]; see
    /// [`Parser::parse_member_type`] for why the count is small and written down.
    ///
    /// The `ref` is recorded on the declaration so the whole statement carries it, which is what
    /// makes the rule *every declarator of a `ref` declaration is itself by reference* true by
    /// construction. Measured, and it is not the obvious reading: `ref int r = ref a[0], s = a[1];`
    /// is CS8172 at the SECOND declarator -- the `ref` distributes to all of them, and the one
    /// without a `ref` initializer is the error, rather than the declaration being half by value.
    ///
    /// **THE GATE FIRES HERE AND NOT AT THE INITIALIZER'S `ref`, ONCE PER DECLARATION.** csc gates
    /// this feature once per declaration however many `ref`s it contains: at `/langversion:6`,
    /// `ref int r = ref a[0], s = ref a[1];` draws ONE CS8059, at the leading `ref`.
    /// [`Parser::in_ref_local_declaration`] is what suppresses the initializers'.
    fn parse_ref_local_type(&mut self) -> TypeRef {
        self.parse_by_ref_type()
    }

    fn ref_is_a_modifier_here(&self) -> bool {
        if !matches!(self.current_keyword(), Some(Keyword::Ref)) {
            return false;
        }
        let next = self.tokens.get(self.position + 1).map(|token| &token.kind);
        if matches!(next, Some(TokenKind::Keyword(Keyword::Struct))) {
            return true;
        }
        matches!(next, Some(TokenKind::Identifier(name)) if &**name == "partial")
            && matches!(
                self.tokens.get(self.position + 2).map(|token| &token.kind),
                Some(TokenKind::Keyword(Keyword::Struct))
            )
    }

    fn partial_is_a_modifier_here(&self) -> bool {
        matches!(self.current_contextual_keyword(), Some("partial"))
            && matches!(
                self.tokens.get(self.position + 1).map(|token| &token.kind),
                Some(TokenKind::Keyword(
                    Keyword::Class | Keyword::Struct | Keyword::Interface
                ))
            )
    }

    /// Whether the identifier `partial` at the current position is a MISPLACED modifier -- one
    /// standing before other modifiers rather than immediately before the declaration's keyword,
    /// which is csc's CS0267.
    ///
    /// **THE LOOKAHEAD HAS TO REACH PAST THE MODIFIERS, AND STOPPING SHORT IS NOT A LOOSER RULE
    /// BUT A WRONG ONE.** `partial x;` is a FIELD of type `partial` and must stay one, so the
    /// identifier alone cannot decide; only a `class`/`struct`/`interface` after the run of
    /// modifier keywords says a type declaration was meant. Without the scan this shape drew a
    /// parse cascade where csc draws one CS0267.
    fn partial_is_misplaced_here(&self) -> bool {
        if !matches!(self.current_contextual_keyword(), Some("partial")) {
            return false;
        }
        let mut ahead = self.position + 1;
        while let Some(TokenKind::Keyword(keyword)) = self.tokens.get(ahead).map(|token| &token.kind)
        {
            match keyword {
                Keyword::Class | Keyword::Struct | Keyword::Interface => return true,
                _ if modifier_of(*keyword).is_some() => ahead += 1,
                _ => return false,
            }
        }
        false
    }

    fn async_is_a_modifier_here(&mut self) -> bool {
        if !matches!(self.current_contextual_keyword(), Some("async")) {
            return false;
        }
        match self.tokens.get(self.position + 1).map(|token| &token.kind) {
            Some(TokenKind::Keyword(keyword)) => {
                if modifier_of(*keyword).is_some() {
                    return true;
                }
                predefined_type(&TokenKind::Keyword(*keyword)).is_some()
                    || matches!(
                        keyword,
                        Keyword::Void
                            | Keyword::Class
                            | Keyword::Struct
                            | Keyword::Interface
                            | Keyword::Enum
                            | Keyword::Delegate
                            | Keyword::Event
                    )
            }
            Some(TokenKind::Identifier(_)) => {
                let saved_position = self.position;
                let saved_diagnostics = self.diagnostics.len();
                self.bump();
                let ty = self.parse_type();
                let is_modifier = !matches!(ty.kind, TypeRefKind::Error)
                    && matches!(self.current().kind, TokenKind::Identifier(_));
                self.position = saved_position;
                self.diagnostics.truncate(saved_diagnostics);
                is_modifier
            }
            _ => false,
        }
    }

    /// Parses a run of leading declaration modifiers (17.2 and elsewhere). The parser accepts any;
    /// binding checks which are valid where.
    fn parse_modifiers(&mut self) -> Vec<Modifier> {
        let mut modifiers = Vec::new();
        loop {
            let modifier = match self.current_keyword().and_then(modifier_of) {
                Some(modifier) => modifier,
                None if self.required_is_a_modifier_here() => {
                    let at = self.current().span;
                    self.gate_feature(Feature::RequiredMembers, at);
                    Modifier::Required
                }
                None if self.ref_is_a_modifier_here() => {
                    let at = self.current().span;
                    self.gate_feature(Feature::RefStruct, at);
                    Modifier::Ref
                }
                None if self.partial_is_a_modifier_here() => {
                    let at = self.current().span;
                    self.gate_feature(Feature::PartialTypes, at);
                    Modifier::Partial
                }
                None if self.partial_is_misplaced_here() => {
                    self.report(DiagnosticKind::PartialModifierPosition, self.current().span);
                    Modifier::Partial
                }
                None if self.async_is_a_modifier_here() => {
                    let at = self.current().span;
                    self.gate_feature(Feature::AsyncFunction, at);
                    Modifier::Async
                }
                None => break,
            };
            if modifiers.contains(&modifier) {
                let span = self.current().span;
                let keyword = token_spelling(&self.current().kind);
                self.report(
                    DiagnosticKind::DuplicateModifier { modifier: keyword },
                    span,
                );
            }
            modifiers.push(modifier);
            self.bump();
        }
        modifiers
    }

    /// Parses one type member (17.2): a nested type, constructor, method,
    /// property, or field. A type keyword begins a nested type; an identifier
    /// directly followed by `(` is a constructor; otherwise a type is parsed, and
    /// a following name then `(` is a method, then `{` is a property, and anything
    /// else is a field.
    fn parse_member(&mut self) -> Member {
        let start = self.current().span.start;
        let attributes = self.parse_attribute_sections();
        let modifiers = self.parse_modifiers();
        if self.record_declaration_here()
            || matches!(
                self.current_keyword(),
                Some(Keyword::Class)
                    | Some(Keyword::Struct)
                    | Some(Keyword::Interface)
                    | Some(Keyword::Enum)
                    | Some(Keyword::Delegate)
            )
        {
            return Member::NestedType(Box::new(
                self.parse_type_kind_declaration(attributes, modifiers, start),
            ));
        }
        let mut member = self.parse_member_body(modifiers, start);
        member.set_attributes(attributes);
        member
    }

    /// Parses a member after its attribute sections and modifiers have been consumed; the
    /// caller attaches the attributes to the result via [`Member::set_attributes`].
    fn parse_member_body(&mut self, modifiers: Vec<Modifier>, start: u32) -> Member {
        if self.current_keyword() == Some(Keyword::Event) {
            return self.parse_event(modifiers, start);
        }
        if matches!(
            self.current_keyword(),
            Some(Keyword::Implicit) | Some(Keyword::Explicit)
        ) {
            return self.parse_conversion_operator(modifiers, start);
        }
        if self.current_punctuator() == Some(Punctuator::Tilde) {
            return self.parse_destructor(modifiers, start);
        }
        if matches!(self.current().kind, TokenKind::Identifier(_))
            && self.next_is(Punctuator::OpenParen)
        {
            let (name, _) = self.expect_identifier();
            self.expect(
                Punctuator::OpenParen,
                DiagnosticKind::TokenExpected { expected: "(" },
            );
            let (parameters, arglist) = self.parse_parameter_sequence(Punctuator::CloseParen);
            let header_end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
            let initializer = if self.eat(Punctuator::Colon) {
                Some(self.parse_constructor_initializer())
            } else {
                None
            };
            let (body, end) = if self.current_punctuator() == Some(Punctuator::EqualsGreaterThan) {
                self.parse_expression_body(Feature::ExpressionBodiedConstructor, false)
            } else {
                let block = self.parse_required_block();
                let end = block.span.end;
                (block, end)
            };
            return Member::Constructor {
                modifiers,
                name,
                parameters,
                is_vararg: arglist.is_some(),
                initializer,
                body,
                header_span: Span::new(start, header_end),
                attributes: Vec::new(),
                span: Span::new(start, end),
            };
        }
        let ty = self.parse_member_type();
        self.in_ref_returning_member = matches!(ty.kind, TypeRefKind::ByRef { .. });
        if self.current_keyword() == Some(Keyword::Operator) {
            return self.parse_operator(modifiers, ty, start);
        }
        if self.current_keyword() == Some(Keyword::This) {
            return self.parse_indexer(modifiers, ty, None, start);
        }
        if matches!(self.current().kind, TokenKind::Identifier(_))
            && (self.next_is(Punctuator::OpenParen)
                || (self.next_is(Punctuator::LessThan)
                    && !self.explicit_interface_qualifier_ahead()))
        {
            let name_span = self.current().span;
            let (name, _) = self.expect_identifier();
            let was_async = self.in_async_method;
            self.in_async_method = modifiers.contains(&Modifier::Async);
            let type_parameters = self.parse_type_parameter_list();
            let (parameters, arglist) = self.parse_parameter_list();
            let constraints = self.parse_type_parameter_constraint_clauses();
            let (body, end) = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                let block = self.parse_block();
                let end = block.span.end;
                (Some(block), end)
            } else if self.current_punctuator() == Some(Punctuator::EqualsGreaterThan) {
                let (block, end) =
                    self.parse_expression_body(Feature::ExpressionBodiedMethod, !Self::type_is_void(&ty));
                (Some(block), end)
            } else {
                if self.in_async_method {
                    self.report(DiagnosticKind::AsyncRequiresBody, name_span);
                }
                let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
                (None, end)
            };
            self.in_async_method = was_async;
            return Member::Method {
                modifiers,
                return_type: ty,
                name,
                name_span,
                type_parameters,
                constraints,
                parameters,
                is_vararg: arglist.is_some(),
                body,
                explicit_interface: None,
                attributes: Vec::new(),
                span: Span::new(start, end),
            };
        }
        if self.explicit_interface_qualifier_ahead() {
            let name_start = self.current().span.start;
            let (first, mut prev_end) = self.expect_identifier();
            let mut parts: Vec<TypeNamePart> = Vec::new();
            let mut name = first;
            let mut arguments: Vec<TypeRef> = Vec::new();
            let mut constructed = false;
            let mut interface_end = prev_end;
            let mut member_span = Span::new(name_start, prev_end);
            if self.current_punctuator() == Some(Punctuator::LessThan)
                && self.generic_type_name_ahead()
            {
                let (list, list_end, _) = self.parse_type_argument_list(false);
                arguments = list;
                constructed = true;
                prev_end = list_end;
            }
            while self.current_punctuator() == Some(Punctuator::Dot) {
                self.bump();
                interface_end = prev_end;
                parts.push(TypeNamePart { name, arguments });
                arguments = Vec::new();
                if self.current_keyword() == Some(Keyword::This) {
                    let explicit_interface = Self::explicit_interface_type(
                        parts,
                        constructed,
                        Span::new(name_start, interface_end),
                    );
                    return self.parse_indexer(modifiers, ty, Some(explicit_interface), start);
                }
                member_span = self.current().span;
                let (part, part_end) = self.expect_identifier();
                name = part;
                prev_end = part_end;
                if self.current_punctuator() == Some(Punctuator::LessThan)
                    && self.generic_type_name_ahead()
                {
                    let (list, list_end, _) = self.parse_type_argument_list(false);
                    arguments = list;
                    constructed = true;
                    prev_end = list_end;
                }
            }
            let member = name;
            let explicit_interface = Self::explicit_interface_type(
                parts,
                constructed,
                Span::new(name_start, interface_end),
            );
            if self.current_punctuator() == Some(Punctuator::OpenBrace)
                || self.current_punctuator() == Some(Punctuator::EqualsGreaterThan)
            {
                return self.parse_property(
                    modifiers,
                    ty,
                    member,
                    member_span,
                    Some(explicit_interface),
                    start,
                );
            }
            let was_async = self.in_async_method;
            self.in_async_method = modifiers.contains(&Modifier::Async);
            let type_parameters = self.parse_type_parameter_list();
            let (parameters, arglist) = self.parse_parameter_list();
            let constraints = self.parse_type_parameter_constraint_clauses();
            let (body, end) = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                let block = self.parse_block();
                let end = block.span.end;
                (Some(block), end)
            } else {
                if self.in_async_method {
                    self.report(DiagnosticKind::AsyncRequiresBody, Span::new(name_start, prev_end));
                }
                let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
                (None, end)
            };
            self.in_async_method = was_async;
            return Member::Method {
                modifiers,
                return_type: ty,
                name: member,
                name_span: member_span,
                type_parameters,
                constraints,
                parameters,
                is_vararg: arglist.is_some(),
                body,
                explicit_interface: Some(explicit_interface),
                attributes: Vec::new(),
                span: Span::new(start, end),
            };
        }
        if matches!(self.current().kind, TokenKind::Identifier(_))
            && (self.next_is(Punctuator::OpenBrace)
                || self.next_is(Punctuator::EqualsGreaterThan))
        {
            let name_span = self.current().span;
            let (name, _) = self.expect_identifier();
            return self.parse_property(modifiers, ty, name, name_span, None, start);
        }
        if matches!(self.current().kind, TokenKind::Identifier(_)) {
            if let TypeRefKind::ByRef { .. } = ty.kind {
                self.gate_feature(Feature::RefFields, Span::empty_at(ty.span.start));
            }
            let declarators = self.parse_variable_declarators(InitializerRefs::Forbidden);
            let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
            return Member::Field {
                modifiers,
                ty,
                declarators,
                attributes: Vec::new(),
                span: Span::new(start, end),
            };
        }
        let span = self.current().span;
        let token = token_spelling(&self.current().kind);
        self.report(
            DiagnosticKind::InvalidTokenInMemberDeclaration { token },
            span,
        );
        self.recover_to_member_boundary();
        Member::Error
    }

    /// After an invalid token in a member declaration, skips to the next member boundary so the
    /// enclosing type-body loop resynchronizes: a `;` ends the bad member (consumed), a `}` ends
    /// the type body (left for the caller to close), and the end of file stops the scan. It also
    /// stops at an identifier -- a token that can begin the next member (a field or method type) --
    /// so a valid member after the junk is re-parsed, and a further stray token there is reported
    /// on its own (csc reports each invalid token, not just the first).
    fn recover_to_member_boundary(&mut self) {
        loop {
            if matches!(
                self.current().kind,
                TokenKind::EndOfFile | TokenKind::Identifier(_)
            ) || self.current_punctuator() == Some(Punctuator::CloseBrace)
            {
                return;
            }
            let ends_member = self.current_punctuator() == Some(Punctuator::Semicolon);
            self.bump();
            if ends_member {
                return;
            }
        }
    }

    /// Parses an accessor body `{ get/set accessors }` (17.6.2, 17.8.2), returning
    /// the `get` and `set` accessors and the byte offset past the closing `}`.
    /// `get` and `set` are contextual identifiers, not keywords, matched by
    /// spelling. Each accessor has a block body or a bare `;`.
    /// Parses an expression body -- `=> expression ;` -- where a member's `{ ... }` block or `;`
    /// would go, and returns it DESUGARED into the block it means (17.6.2, C# 6.0).
    ///
    /// **A DESUGAR RATHER THAN AN AST NODE, AND THAT IS THE WHOLE REASON THIS FEATURE IS CHEAP.**
    /// `int M() => e;` is `int M() { return e; }` and nothing downstream needs to know the
    /// difference: the binder, the flow analysis and the emitter see the block they already
    /// handle. A node would have meant a new case in every one of them.
    ///
    /// `returns_value` decides which statement the block holds: a member with a value returns it,
    /// and a `void` method or a `set`/`add`/`remove` accessor evaluates the expression as a
    /// statement. Getting that backwards is not cosmetic -- `return e;` in a void method is
    /// `CS0127`, and a bare `e;` in a value-returning one is `CS0161`.
    ///
    /// `feature` names the member kind, because csc's message does: *'expression-bodied method'*
    /// and *'expression-bodied property'* are different search keys at the same version.
    fn parse_expression_body(&mut self, feature: Feature, returns_value: bool) -> (Stmt, u32) {
        let arrow = self.current().span;
        self.gate_feature(feature, arrow);
        self.bump();
        let expression = if self.current_keyword() == Some(Keyword::Ref) {
            let at = self.current().span;
            self.bump();
            if !self.in_ref_returning_member {
                self.gate_feature(Feature::ByRefLocalsAndReturns, at);
            }
            let operand = self.parse_expression();
            let span = Span::new(at.start, operand.span.end);
            Expr::new(
                ExprKind::RefArgument {
                    position: RefPosition::Return,
                    out: false,
                    operand: Box::new(operand),
                },
                span,
            )
        } else {
            self.parse_expression()
        };
        let expression_span = expression.span;
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        let inner = match expression.kind {
            ExprKind::Throw(operand) => Stmt::new(StmtKind::Throw(Some(*operand)), expression_span),
            _ if returns_value => Stmt::new(StmtKind::Return(Some(expression)), expression_span),
            _ => Stmt::new(StmtKind::Expression(expression), expression_span),
        };
        let span = Span::new(arrow.start, end);
        (Stmt::new(StmtKind::Block(alloc::vec![inner]), span), end)
    }

    /// Reports that `feature` needs a later dialect than the one being compiled, and says nothing
    /// when the dialect already has it. One place, because the message carries the feature's csc
    /// NAME and its introducing version together and a caller that assembled them itself could
    /// pair the right name with the wrong version.
    /// Refuses `feature` at `at` unless this compilation can admit it, and says WHICH of the two
    /// bits refused it.
    ///
    /// **THE ONLY PLACE IN THE PARSER THAT RAISES A FEATURE GATE.** The decision itself is
    /// [`Feature::gate_against`]'s, shared with the binder's `gate_feature`, so the two halves of
    /// the compiler cannot drift into two answers.
    ///
    fn gate_feature(&mut self, feature: Feature, at: Span) {
        let kind = match feature.gate_against(self.version) {
            None => return,
            Some(FeatureGate::RequiresLaterVersion { required }) => {
                DiagnosticKind::FeatureRequiresLaterVersion {
                    feature: feature.description(),
                    required,
                    current: self.version,
                }
            }
            Some(FeatureGate::NotInThisBuild) => DiagnosticKind::FeatureNotInThisBuild {
                feature: feature.description(),
                permitted_by: self.version,
            },
        };
        self.report(kind, at);
    }

    /// [`Self::gate_feature`] at the current token's start, as an empty span.
    ///
    /// The position most gate sites want: csc reports a feature gate at the construct's first
    /// character rather than over its extent.
    fn gate_feature_here(&mut self, feature: Feature) {
        let at = Span::empty_at(self.current().span.start);
        self.gate_feature(feature, at);
    }

    /// Whether a syntactic return type is `void`, which decides whether an expression body
    /// returns its expression or evaluates it. Syntactic on purpose: the parser has no model, and
    /// `void` is a keyword rather than a name, so there is nothing to resolve.
    fn type_is_void(ty: &TypeRef) -> bool {
        matches!(&ty.kind, TypeRefKind::Predefined(PredefinedType::Void))
    }

    fn parse_accessor_block(&mut self) -> (Option<Accessor>, Option<Accessor>, u32) {
        self.expect(Punctuator::OpenBrace, DiagnosticKind::OpenBraceExpected);
        let mut getter = None;
        let mut setter = None;
        loop {
            if self.current_punctuator() == Some(Punctuator::CloseBrace)
                || matches!(self.current().kind, TokenKind::EndOfFile)
            {
                break;
            }
            let accessor_start = self.current().span.start;
            let attributes = self.parse_attribute_sections();
            let mut modifiers = Vec::new();
            while let Some(modifier) = match self.current_keyword() {
                Some(Keyword::Public) => Some(Modifier::Public),
                Some(Keyword::Protected) => Some(Modifier::Protected),
                Some(Keyword::Internal) => Some(Modifier::Internal),
                Some(Keyword::Private) => Some(Modifier::Private),
                _ => None,
            } {
                modifiers.push(modifier);
                self.bump();
            }
            let accessor_keyword_span = self.current().span;
            let (is_getter, is_init) = match self.current_contextual_keyword() {
                Some("get") => (true, false),
                Some("set") => (false, false),
                Some("init") => (false, true),
                _ => break,
            };
            if is_init {
                self.gate_feature(Feature::InitOnlySetters, accessor_keyword_span);
            }
            self.bump();
            let (body, accessor_end) = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                let block = self.parse_block();
                let end = block.span.end;
                (Some(block), end)
            } else if self.current_punctuator() == Some(Punctuator::EqualsGreaterThan) {
                let (block, end) =
                    self.parse_expression_body(Feature::ExpressionBodiedAccessor, is_getter);
                (Some(block), end)
            } else {
                let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
                (None, end)
            };
            let accessor = Accessor {
                attributes,
                modifiers,
                body,
                is_init,
                span: Span::new(accessor_start, accessor_end),
            };
            let already = if is_getter { getter.is_some() } else { setter.is_some() };
            if already {
                self.report(DiagnosticKind::DuplicateAccessor, accessor_keyword_span);
            } else if is_getter {
                getter = Some(accessor);
            } else {
                setter = Some(accessor);
            }
        }
        let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
        (getter, setter, end)
    }

    /// Parses a property given the modifiers, type, and name already parsed (17.6), and an
    /// explicitly implemented interface for `int I.P { ... }` (20.4.1), else `None`.
    fn parse_property(
        &mut self,
        modifiers: Vec<Modifier>,
        ty: TypeRef,
        name: Box<str>,
        name_span: Span,
        explicit_interface: Option<TypeRef>,
        start: u32,
    ) -> Member {
        let (getter, setter, end) =
            if self.current_punctuator() == Some(Punctuator::EqualsGreaterThan) {
                let accessor_start = self.current().span.start;
                let (block, end) =
                    self.parse_expression_body(Feature::ExpressionBodiedProperty, true);
                let getter = Accessor {
                    attributes: Vec::new(),
                    modifiers: Vec::new(),
                    body: Some(block),
                    is_init: false,
                    span: Span::new(accessor_start, end),
                };
                (Some(getter), None, end)
            } else {
                self.parse_accessor_block()
            };
        let (initializer, end) = if self.current_punctuator() == Some(Punctuator::Equals) {
            let at = self.current().span;
            self.bump();
            self.gate_feature(Feature::AutoPropertyInitializer, at);
            let value = self.parse_expression();
            (
                Some(value),
                self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected),
            )
        } else {
            (None, end)
        };
        if let Some(accessor) = &setter {
            if accessor.is_init && modifiers.iter().any(|m| matches!(m, Modifier::Static)) {
                self.report(DiagnosticKind::InitAccessorOnStaticMember, accessor.span);
            }
        }
        Member::Property {
            modifiers,
            ty,
            name,
            name_span,
            getter,
            setter,
            explicit_interface,
            initializer,
            attributes: Vec::new(),
            span: Span::new(start, end),
        }
    }

    /// Parses an event member given the modifiers already parsed (17.7): the
    /// `event` keyword, a type, then either a field-like declarator list ending in
    /// `;` or a `{ add/remove }` accessor block.
    fn parse_event(&mut self, modifiers: Vec<Modifier>, start: u32) -> Member {
        self.bump();
        let ty = self.parse_type();
        if matches!(self.current().kind, TokenKind::Identifier(_)) && self.next_is(Punctuator::Dot)
        {
            let name_start = self.current().span.start;
            let (first, mut prev_end) = self.expect_identifier();
            let mut parts = alloc::vec![first];
            let mut interface_end = prev_end;
            while self.current_punctuator() == Some(Punctuator::Dot) {
                self.bump();
                interface_end = prev_end;
                let (part, part_end) = self.expect_identifier();
                parts.push(part);
                prev_end = part_end;
            }
            let name = parts.pop().expect("a qualified member name has >= 2 parts");
            let explicit_interface =
                TypeRef::new(TypeRefKind::Name(parts), Span::new(name_start, interface_end));
            let (adder, remover, end) = self.parse_event_accessor_block();
            return Member::Event {
                modifiers,
                ty,
                name,
                adder,
                remover,
                explicit_interface: Some(explicit_interface),
                attributes: Vec::new(),
                span: Span::new(start, end),
            };
        }
        if matches!(self.current().kind, TokenKind::Identifier(_))
            && self.next_is(Punctuator::OpenBrace)
        {
            let (name, _) = self.expect_identifier();
            let (adder, remover, end) = self.parse_event_accessor_block();
            return Member::Event {
                modifiers,
                ty,
                name,
                adder,
                remover,
                explicit_interface: None,
                attributes: Vec::new(),
                span: Span::new(start, end),
            };
        }
        let declarators = self.parse_variable_declarators(InitializerRefs::Forbidden);
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        Member::EventField {
            modifiers,
            ty,
            declarators,
            attributes: Vec::new(),
            span: Span::new(start, end),
        }
    }

    /// Parses an event's `{ add ... remove ... }` accessor block (17.7.1). `add`
    /// and `remove` are contextual identifiers; each accessor has a block body.
    fn parse_event_accessor_block(&mut self) -> (Option<Accessor>, Option<Accessor>, u32) {
        self.expect(Punctuator::OpenBrace, DiagnosticKind::OpenBraceExpected);
        let mut adder = None;
        let mut remover = None;
        loop {
            if self.current_punctuator() == Some(Punctuator::CloseBrace)
                || matches!(self.current().kind, TokenKind::EndOfFile)
            {
                break;
            }
            let accessor_start = self.current().span.start;
            let attributes = self.parse_attribute_sections();
            let is_adder = match self.current_contextual_keyword() {
                Some("add") => true,
                Some("remove") => false,
                _ => break,
            };
            self.bump();
            let (body, accessor_end) = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                let block = self.parse_block();
                let end = block.span.end;
                (Some(block), end)
            } else {
                let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
                (None, end)
            };
            let accessor = Accessor {
                attributes,
                modifiers: Vec::new(),
                body,
                is_init: false,
                span: Span::new(accessor_start, accessor_end),
            };
            if is_adder {
                adder = Some(accessor);
            } else {
                remover = Some(accessor);
            }
        }
        let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
        (adder, remover, end)
    }

    /// Parses an overloaded operator given the modifiers and return type already
    /// parsed (17.9): the `operator` keyword, an overloadable operator, a parameter
    /// list, then a body.
    fn parse_operator(
        &mut self,
        modifiers: Vec<Modifier>,
        return_type: TypeRef,
        start: u32,
    ) -> Member {
        self.bump();
        let operator = match self.overloadable_operator() {
            Some(operator) => {
                self.bump();
                operator
            }
            None => {
                let at = self.current().span.start;
                self.report(
                    DiagnosticKind::OverloadableOperatorExpected,
                    Span::empty_at(at),
                );
                OverloadableOperator::Plus
            }
        };
        let (parameters, arglist) = self.parse_parameter_list();
        if let Some(span) = arglist {
            self.report(DiagnosticKind::ArglistNotValidInThisContext, span);
        }
        let (body, end) = if self.current_punctuator() == Some(Punctuator::EqualsGreaterThan) {
            self.parse_expression_body(Feature::ExpressionBodiedMethod, true)
        } else {
            let block = self.parse_required_block();
            let end = block.span.end;
            (block, end)
        };
        Member::Operator {
            modifiers,
            return_type,
            operator,
            parameters,
            body,
            attributes: Vec::new(),
            span: Span::new(start, end),
        }
    }

    /// Parses a user-defined conversion operator given the modifiers already
    /// parsed (17.9.3): `implicit`/`explicit`, `operator`, a target type, a
    /// parameter list, then a body.
    fn parse_conversion_operator(&mut self, modifiers: Vec<Modifier>, start: u32) -> Member {
        let direction = if self.eat_keyword(Keyword::Implicit) {
            ConversionDirection::Implicit
        } else {
            self.bump();
            ConversionDirection::Explicit
        };
        self.expect_keyword(Keyword::Operator, "operator");
        let target = self.parse_type();
        let (parameters, arglist) = self.parse_parameter_list();
        if let Some(span) = arglist {
            self.report(DiagnosticKind::ArglistNotValidInThisContext, span);
        }
        let (body, end) = if self.current_punctuator() == Some(Punctuator::EqualsGreaterThan) {
            self.parse_expression_body(Feature::ExpressionBodiedMethod, true)
        } else {
            let block = self.parse_required_block();
            let end = block.span.end;
            (block, end)
        };
        Member::ConversionOperator {
            modifiers,
            direction,
            target,
            parameters,
            body,
            attributes: Vec::new(),
            span: Span::new(start, end),
        }
    }

    /// The overloadable operator the current token denotes, if any (17.9). `true`
    /// and `false` are keyword operators; the rest are punctuators.
    fn overloadable_operator(&self) -> Option<OverloadableOperator> {
        if let Some(punctuator) = self.current_punctuator() {
            return Some(match punctuator {
                Punctuator::Plus => OverloadableOperator::Plus,
                Punctuator::Minus => OverloadableOperator::Minus,
                Punctuator::Exclamation => OverloadableOperator::LogicalNot,
                Punctuator::Tilde => OverloadableOperator::BitwiseNot,
                Punctuator::PlusPlus => OverloadableOperator::Increment,
                Punctuator::MinusMinus => OverloadableOperator::Decrement,
                Punctuator::Asterisk => OverloadableOperator::Multiply,
                Punctuator::Slash => OverloadableOperator::Divide,
                Punctuator::Percent => OverloadableOperator::Remainder,
                Punctuator::Ampersand => OverloadableOperator::BitwiseAnd,
                Punctuator::Bar => OverloadableOperator::BitwiseOr,
                Punctuator::Caret => OverloadableOperator::ExclusiveOr,
                Punctuator::LessThanLessThan => OverloadableOperator::LeftShift,
                Punctuator::GreaterThanGreaterThan => OverloadableOperator::RightShift,
                Punctuator::EqualsEquals => OverloadableOperator::Equality,
                Punctuator::ExclamationEquals => OverloadableOperator::Inequality,
                Punctuator::GreaterThan => OverloadableOperator::GreaterThan,
                Punctuator::LessThan => OverloadableOperator::LessThan,
                Punctuator::GreaterThanEquals => OverloadableOperator::GreaterThanOrEqual,
                Punctuator::LessThanEquals => OverloadableOperator::LessThanOrEqual,
                _ => return None,
            });
        }
        match self.current_keyword() {
            Some(Keyword::True) => Some(OverloadableOperator::True),
            Some(Keyword::False) => Some(OverloadableOperator::False),
            _ => None,
        }
    }

    /// Parses a destructor given the modifiers already parsed (17.12): `~ name
    /// ( ) body`.
    fn parse_destructor(&mut self, modifiers: Vec<Modifier>, start: u32) -> Member {
        self.bump();
        let (name, _) = self.expect_identifier();
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        let (body, end) = if self.current_punctuator() == Some(Punctuator::EqualsGreaterThan) {
            self.parse_expression_body(Feature::ExpressionBodiedConstructor, false)
        } else {
            let block = self.parse_required_block();
            let end = block.span.end;
            (block, end)
        };
        Member::Destructor {
            modifiers,
            name,
            body,
            attributes: Vec::new(),
            span: Span::new(start, end),
        }
    }

    /// Parses a constructor initializer (17.10.1), the scanner just past the `:`:
    /// `base ( args )` or `this ( args )`.
    fn parse_constructor_initializer(&mut self) -> ConstructorInitializer {
        let span = self.current().span;
        let kind = if self.eat_keyword(Keyword::Base) {
            ConstructorInitializerKind::Base
        } else if self.eat_keyword(Keyword::This) {
            ConstructorInitializerKind::This
        } else {
            let at = self.current().span.start;
            self.report(
                DiagnosticKind::TokenExpected { expected: "base" },
                Span::empty_at(at),
            );
            ConstructorInitializerKind::Base
        };
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        let (arguments, _) =
            self.parse_arguments(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        ConstructorInitializer {
            kind,
            arguments,
            span,
        }
    }

    /// Parses an indexer given the modifiers and type already parsed (17.8): the
    /// `this` keyword, a bracketed index parameter list, then an accessor body.
    /// The `TypeRef` for an explicit interface implementation's QUALIFIER, from the parts read
    /// off the member name and whether any of them carried type arguments.
    ///
    /// **ONE STATEMENT OF THE RULE, FOR THE THREE MEMBER KINDS THAT REACH IT.** A method, a
    /// property and an indexer each stop the qualifier loop at a different token, so each needs
    /// the same `parts` turned into the same type -- and a second spelling of "generic when any
    /// part is constructed" is exactly the shape that gains a case in one position and not the
    /// others.
    fn explicit_interface_type(
        parts: Vec<TypeNamePart>,
        constructed: bool,
        span: Span,
    ) -> TypeRef {
        let kind = if constructed {
            TypeRefKind::Generic { parts }
        } else {
            TypeRefKind::Name(parts.into_iter().map(|part| part.name).collect())
        };
        TypeRef::new(kind, span)
    }

    /// Parses an indexer (17.8) from its `this` keyword on, with `ty` its element type.
    /// `explicit_interface` is the qualifier of `int I.this[int i]` (20.4.1) and `None` for an
    /// ordinary indexer.
    fn parse_indexer(
        &mut self,
        modifiers: Vec<Modifier>,
        ty: TypeRef,
        explicit_interface: Option<TypeRef>,
        start: u32,
    ) -> Member {
        let name_span = self.current().span;
        self.bump();
        self.expect(
            Punctuator::OpenBracket,
            DiagnosticKind::TokenExpected { expected: "[" },
        );
        let (parameters, arglist) = self.parse_parameter_sequence(Punctuator::CloseBracket);
        if let Some(span) = arglist {
            self.report(DiagnosticKind::ArglistNotValidInThisContext, span);
        }
        self.expect(
            Punctuator::CloseBracket,
            DiagnosticKind::TokenExpected { expected: "]" },
        );
        let (getter, setter, end) =
            if self.current_punctuator() == Some(Punctuator::EqualsGreaterThan) {
                let accessor_start = self.current().span.start;
                let (block, end) =
                    self.parse_expression_body(Feature::ExpressionBodiedIndexer, true);
                let getter = Accessor {
                    attributes: Vec::new(),
                    modifiers: Vec::new(),
                    body: Some(block),
                    is_init: false,
                    span: Span::new(accessor_start, end),
                };
                (Some(getter), None, end)
            } else {
                self.parse_accessor_block()
            };
        if self.current_punctuator() == Some(Punctuator::Equals) {
            let span = self.current().span;
            let token = token_spelling(&self.current().kind);
            self.report(
                DiagnosticKind::InvalidTokenInMemberDeclaration { token },
                span,
            );
            self.recover_to_member_boundary();
        }
        Member::Indexer {
            modifiers,
            ty,
            parameters,
            name_span,
            getter,
            setter,
            explicit_interface,
            attributes: Vec::new(),
            span: Span::new(start, end),
        }
    }

    /// Parses a parenthesized formal-parameter list (17.5.1).
    fn parse_parameter_list(&mut self) -> (Vec<Parameter>, Option<Span>) {
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        let parameters = self.parse_parameter_sequence(Punctuator::CloseParen);
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        parameters
    }

    /// Parses a comma-separated formal-parameter sequence up to `close`, without
    /// consuming the surrounding brackets. Shared by parameter lists `( )` and
    /// indexer index lists `[ ]`. The second element is the span of a trailing
    /// `__arglist` vararg marker when one was present (tokenized only under the
    /// typedref knob): a method/constructor list accepts it, any other context
    /// reports CS1669 at that span. A non-final `__arglist` is CS0257 here, where
    /// the position is known, and the marker still registers so the member keeps
    /// its vararg shape for downstream binding.
    fn parse_parameter_sequence(&mut self, close: Punctuator) -> (Vec<Parameter>, Option<Span>) {
        let mut parameters = Vec::new();
        let mut arglist: Option<Span> = None;
        if self.current_punctuator() == Some(close) {
            return (parameters, arglist);
        }
        loop {
            let start = self.current().span.start;
            let attributes = self.parse_attribute_sections();
            if let Some(span) = Self::caller_info_attribute_span(&attributes) {
                self.gate_feature(Feature::CallerInfoAttribute, span);
            }
            if self.current().kind == TokenKind::TypedRefKeyword(TypedRefKeyword::ArgList) {
                let span = self.current().span;
                self.bump();
                if arglist.is_none() {
                    arglist = Some(span);
                }
                if self.eat(Punctuator::Comma) {
                    self.report(DiagnosticKind::ArglistMustBeLast, span);
                    continue;
                }
                break;
            }
            let modifier = match self.current_keyword() {
                Some(Keyword::Ref) => {
                    self.bump();
                    Some(ParameterModifier::Ref)
                }
                Some(Keyword::Out) => {
                    self.bump();
                    Some(ParameterModifier::Out)
                }
                Some(Keyword::Params) => {
                    self.bump();
                    Some(ParameterModifier::Params)
                }
                _ => None,
            };
            let ty = self.parse_type();
            let (name, mut end) = self.expect_declared_name();
            let mut default_value = None;
            if self.current_punctuator() == Some(Punctuator::Equals) {
                let at = self.current().span.start;
                self.gate_feature(Feature::DefaultParameterValues, Span::empty_at(at));
                self.bump();
                let value = self.parse_expression();
                end = value.span.end;
                default_value = Some(value);
            }
            parameters.push(Parameter {
                modifier,
                ty,
                name,
                default_value,
                span: Span::new(start, end),
            });
            if !self.eat(Punctuator::Comma) {
                break;
            }
        }
        (parameters, arglist)
    }

    /// The span of a CALLER-INFO ATTRIBUTE among `sections`, if one is present.
    ///
    /// `[CallerMemberName]` and its two siblings substitute a constant for an OMITTED argument at
    /// the call site, and that substitution is the whole feature. Accepting the attribute and
    /// ignoring it compiles, and then passes the DECLARED default everywhere -- so a logging
    /// helper built on it records `null` instead of its caller. A wrong answer, not a missing one.
    ///
    ///
    /// Matched on the WRITTEN name, with and without the `Attribute` suffix and ignoring any
    /// qualification, because the parser has no types -- and every spelling of these three reduces
    /// to one of six final identifiers.
    fn caller_info_attribute_span(sections: &[AttributeSection]) -> Option<Span> {
        const NAMES: [&str; 6] = [
            "CallerMemberName",
            "CallerMemberNameAttribute",
            "CallerFilePath",
            "CallerFilePathAttribute",
            "CallerLineNumber",
            "CallerLineNumberAttribute",
        ];
        sections
            .iter()
            .flat_map(|section| section.attributes.iter())
            .find(|attribute| {
                attribute
                    .name
                    .parts
                    .last()
                    .is_some_and(|part| NAMES.contains(&&**part))
            })
            .map(|attribute| attribute.span)
    }

    /// Parses a full expression (14): an assignment, which sits at the bottom of
    /// the precedence ladder.
    fn parse_expression(&mut self) -> Expr {
        self.parse_assignment()
    }

    /// Assignment (14.14), right-associative and lower than the conditional. The
    /// target is parsed as a conditional and validated as an lvalue when binding,
    /// matching how csc parses then checks.
    /// Whether a LAMBDA starts at the current token, and if so parses it (14.5.11).
    ///
    /// **THE LOOKAHEAD IS THE WHOLE DESIGN.** Two shapes begin a lambda and only one is decidable
    /// from a single token:
    ///
    ///   `x => ...`            an identifier followed by `=>`, decidable at distance 1
    ///   `( ... ) => ...`      decidable only by SCANNING TO THE MATCHING `)`, because everything
    ///                         inside is also a legal parenthesized expression
    ///
    /// The scan counts nesting over `(`/`)` and `[`/`]` and stops at the matching close, then asks
    /// whether `=>` follows. It reads tokens and consumes none, so a `(a + b)` that is NOT a lambda
    /// costs one linear scan and then parses normally.
    ///
    /// `async x => ...` IS NOT HANDLED HERE. An async lambda is a C# 5.0 feature of its own with
    /// its own state machine, and `async` is contextual -- reading it here would claim a feature
    /// this returns no tree for.
    fn try_parse_lambda(&mut self) -> Option<Expr> {
        let start = self.current().span.start;
        if matches!(self.current().kind, TokenKind::Identifier(_))
            && self.next_is(Punctuator::EqualsGreaterThan)
        {
            let (name, end) = self.expect_identifier();
            let parameters = alloc::vec![LambdaParameter {
                ty: None,
                name,
                span: Span::new(start, end),
            }];
            return Some(self.finish_lambda(parameters, false, start));
        }
        if self.current_punctuator() != Some(Punctuator::OpenParen) || !self.arrow_follows_group() {
            return None;
        }
        self.bump();
        let mut parameters = Vec::new();
        while self.current_punctuator() != Some(Punctuator::CloseParen)
            && !matches!(self.current().kind, TokenKind::EndOfFile)
        {
            let at = self.current().span.start;
            let implicit = matches!(self.current().kind, TokenKind::Identifier(_))
                && (self.next_is(Punctuator::Comma) || self.next_is(Punctuator::CloseParen));
            let ty = if implicit { None } else { Some(self.parse_type()) };
            let (name, end) = self.expect_declared_name();
            parameters.push(LambdaParameter {
                ty,
                name,
                span: Span::new(at, end),
            });
            if self.current_punctuator() == Some(Punctuator::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        Some(self.finish_lambda(parameters, true, start))
    }

    /// Whether the parenthesized group starting at the current `(` is followed by `=>`.
    ///
    /// Reads ahead without consuming. The nesting count covers `(` and `[` together: a default
    /// argument or an indexer inside a parameter list can carry either, and a scan that tracked
    /// only parentheses would stop at the first `)` inside `f(a[b(c)])`.
    fn arrow_follows_group(&self) -> bool {
        let mut depth = 0usize;
        let mut at = self.position;
        loop {
            let Some(token) = self.tokens.get(at) else {
                return false;
            };
            match &token.kind {
                TokenKind::EndOfFile => return false,
                TokenKind::Punctuator(Punctuator::OpenParen | Punctuator::OpenBracket) => {
                    depth += 1;
                }
                TokenKind::Punctuator(Punctuator::CloseParen | Punctuator::CloseBracket) => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(
                            self.tokens.get(at + 1).map(|token| &token.kind),
                            Some(TokenKind::Punctuator(Punctuator::EqualsGreaterThan))
                        );
                    }
                }
                _ => {}
            }
            at += 1;
        }
    }

    /// The shared tail of both lambda spellings: the `=>`, its gate, and the body.
    fn finish_lambda(
        &mut self,
        parameters: Vec<LambdaParameter>,
        parenthesized: bool,
        start: u32,
    ) -> Expr {
        let arrow = self.current().span;
        self.gate_feature(Feature::LambdaExpression, arrow);
        self.expect(
            Punctuator::EqualsGreaterThan,
            DiagnosticKind::TokenExpected { expected: "=>" },
        );
        let body = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
            LambdaBody::Block(self.parse_block())
        } else {
            LambdaBody::Expression(self.parse_expression())
        };
        let end = match &body {
            LambdaBody::Expression(expr) => expr.span.end,
            LambdaBody::Block(block) => block.span.end,
        };
        Expr::new(
            ExprKind::Lambda {
                parameters,
                body: Box::new(body),
                parenthesized,
            },
            Span::new(start, end),
        )
    }

    fn parse_assignment(&mut self) -> Expr {
        if let Some(lambda) = self.try_parse_lambda() {
            return lambda;
        }
        if let Some(deconstruction) = self.try_parse_deconstruction() {
            return deconstruction;
        }
        let target = self.parse_conditional();
        let Some(operator) = self.current_punctuator().and_then(assignment_operator) else {
            return target;
        };
        self.bump();
        let value = if operator == AssignmentOperator::Assign
            && self.current_keyword() == Some(Keyword::Ref)
        {
            let at = self.current().span;
            self.bump();
            self.gate_feature(Feature::RefReassignment, at);
            let operand = self.parse_assignment();
            let span = Span::new(at.start, operand.span.end);
            Expr::new(
                ExprKind::RefArgument {
                    out: false,
                    position: RefPosition::Reassignment,
                    operand: Box::new(operand),
                },
                span,
            )
        } else {
            self.parse_assignment()
        };
        let span = Span::new(target.span.start, value.span.end);
        Expr::new(
            ExprKind::Assignment {
                operator,
                target: Box::new(target),
                value: Box::new(value),
            },
            span,
        )
    }

    /// The conditional `a ? b : c` (14.13). Its branches are full expressions, so
    /// they may themselves be assignments or further conditionals.
    fn parse_conditional(&mut self) -> Expr {
        let condition = self.parse_null_coalescing();
        if !self.eat(Punctuator::Question) {
            return condition;
        }
        let when_true = self.parse_expression();
        self.expect(
            Punctuator::Colon,
            DiagnosticKind::TokenExpected { expected: ":" },
        );
        let when_false = self.parse_expression();
        let span = Span::new(condition.span.start, when_false.span.end);
        Expr::new(
            ExprKind::Conditional {
                condition: Box::new(condition),
                when_true: Box::new(when_true),
                when_false: Box::new(when_false),
            },
            span,
        )
    }

    /// The null-coalescing operator `a ?? b` (C# 2.0; ECMA-334 4th ed 14.13), which sits between
    /// conditional-OR and the conditional operator.
    ///
    /// **RIGHT-ASSOCIATIVE, AND THAT IS NOT A STYLE CHOICE**: `a ?? b ?? c` groups as
    /// `a ?? (b ?? c)`, and the other grouping differs whenever `b` is null -- `(a ?? b) ?? c`
    /// tests the RESULT of the first coalesce, so a null `b` reached with a null `a` would yield
    /// `c` either way but a null `b` with a non-null `a` would not. Recursing at this level rather
    /// than looping is what makes it right-associative.
    fn parse_null_coalescing(&mut self) -> Expr {
        let left = if self.current_keyword() == Some(Keyword::Throw) {
            let start = self.current().span;
            self.gate_feature(Feature::ThrowExpression, start);
            self.bump();
            let operand = self.parse_binary(1);
            let span = Span::new(start.start, operand.span.end);
            Expr::new(ExprKind::Throw(Box::new(operand)), span)
        } else {
            self.parse_binary(1)
        };
        if self.current_punctuator() != Some(Punctuator::QuestionQuestion) {
            return left;
        }
        self.bump();
        let right = self.parse_null_coalescing();
        let span = Span::new(left.span.start, right.span.end);
        Expr::new(
            ExprKind::NullCoalescing {
                left: Box::new(left),
                right: Box::new(right),
            },
            span,
        )
    }

    /// The binary operators (14.7 through 14.12) by precedence climbing.
    /// `minimum` is the lowest precedence this call will accept; all the
    /// operators are left-associative.
    fn parse_binary(&mut self, minimum: u8) -> Expr {
        let mut left = self.parse_unary();
        loop {
            if SWITCH_PRECEDENCE >= minimum
                && matches!(self.current().kind, TokenKind::Keyword(Keyword::Switch))
                && self.next_is(Punctuator::OpenBrace)
            {
                left = self.parse_switch_expression(left);
                continue;
            }
            if RELATIONAL_PRECEDENCE >= minimum {
                if let Some(operation) = type_test_operation(&self.current().kind) {
                    let operator = self.current().span;
                    self.bump();
                    let target = match operation {
                        TypeTestOperation::Is => {
                            self.parse_pattern(operator, PatternPosition::TypeTest)
                        }
                        TypeTestOperation::As => Pattern::Type(self.parse_is_target_type()),
                    };
                    let end = pattern_end(&target);
                    let span = Span::new(left.span.start, end);
                    left = Expr::new(
                        ExprKind::TypeTest {
                            operation,
                            operand: Box::new(left),
                            target,
                        },
                        span,
                    );
                    continue;
                }
            }
            let Some((operator, precedence)) = self.current_punctuator().and_then(binary_operator)
            else {
                break;
            };
            if precedence < minimum {
                break;
            }
            self.bump();
            let right = self.parse_binary(precedence + 1);
            let span = Span::new(left.span.start, right.span.end);
            left = Expr::new(
                ExprKind::Binary {
                    operator,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
            );
        }
        left
    }

    /// Parses a PATTERN, in the position `position` names; `operator` is the span of the token
    /// that introduced it (the `is`, or the `switch`).
    ///
    /// **THE ORDER OF THE TESTS IS THE GRAMMAR.** `null` is a keyword and can begin nothing else;
    /// a token that cannot begin a TYPE settles every other literal without speculation. Only a
    /// NAME is genuinely ambiguous, and the two positions answer that ambiguity differently.
    ///
    /// **THE POSITION IS A PARAMETER RATHER THAN A SECOND FUNCTION, BECAUSE THE TWO READINGS
    /// DIFFER IN EXACTLY TWO PLACES AND SHARE EVERYTHING ELSE** -- a bare `_`, and a name with no
    /// designator. After `is` both are C# 1.0 TYPES (measured: `o is _` is `CS0246` under csc at
    /// every version); in a switch arm the first is a discard and the second a CONSTANT, which is
    /// what an enum-member arm is. Two spellings of the shared part is the shape where one of them
    /// gains a case and the other does not.
    ///
    /// **THE FEATURE GATE IS REPORTED AT THE INTRODUCING TOKEN, NOT AT THE PATTERN, AND THAT IS
    /// MEASURED.** csc's `CS8059` for `o is string s` at `/langversion:6` points at the `is`
    /// keyword, and so does the one for `x is 3` -- one position for every pattern form.
    /// Reporting at the construct that DREW the gate reads as the natural choice and puts the
    /// caret ten columns late on a declaration pattern.
    fn parse_pattern(&mut self, operator: Span, position: PatternPosition) -> Pattern {
        if matches!(self.current().kind, TokenKind::Keyword(Keyword::Null)) {
            let at = self.current().span;
            self.gate_feature(Feature::ConstantPattern, operator);
            self.bump();
            return Pattern::Constant(Box::new(Expr::new(ExprKind::Literal(Literal::Null), at)));
        }
        if position == PatternPosition::SwitchArm && self.at_discard_pattern(position) {
            let at = self.current().span;
            self.bump();
            self.gate_feature(Feature::SwitchExpression, at);
            return Pattern::Discard(at);
        }
        if !self.at_type_start() {
            return self.parse_constant_pattern(operator);
        }
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        let ty = self.parse_is_target_type();
        if !matches!(ty.kind, TypeRefKind::Error) && self.at_pattern_designator(position) {
            let TokenKind::Identifier(name) = &self.current().kind else {
                unreachable!("the designator predicate matched an identifier")
            };
            let name = name.clone();
            let at = self.current().span;
            self.bump();
            self.gate_feature(Feature::DeclarationPattern, operator);
            return Pattern::Declaration { ty, name, span: at };
        }
        if position == PatternPosition::TypeTest {
            return Pattern::Type(ty);
        }
        self.position = saved_position;
        self.diagnostics.truncate(saved_diagnostics);
        self.parse_constant_pattern(operator)
    }

    /// Parses a SWITCH EXPRESSION's `switch { arm, ... }`, with `governing` already built and the
    /// cursor on the `switch`.
    ///
    /// **A TRAILING COMMA IS LEGAL AND AN EMPTY ARM LIST IS TOO** -- both measured: `x switch { }`
    /// compiles under csc with `CS8509`, so an empty list is an exhaustiveness question rather
    /// than a syntax error, and refusing it here would report the wrong thing.
    fn parse_switch_expression(&mut self, governing: Expr) -> Expr {
        let operator = self.current().span;
        self.bump();
        self.gate_feature(Feature::SwitchExpression, operator);
        self.expect(Punctuator::OpenBrace, DiagnosticKind::OpenBraceExpected);
        let mut arms = Vec::new();
        while self.current_punctuator() != Some(Punctuator::CloseBrace)
            && !matches!(self.current().kind, TokenKind::EndOfFile)
        {
            let before = self.position;
            let pattern = self.parse_pattern(operator, PatternPosition::SwitchArm);
            let guard = if matches!(&self.current().kind, TokenKind::Identifier(name) if &**name == "when")
            {
                self.bump();
                Some(self.parse_null_coalescing())
            } else {
                None
            };
            self.expect(
                Punctuator::EqualsGreaterThan,
                DiagnosticKind::TokenExpected { expected: "=>" },
            );
            let value = self.parse_expression();
            arms.push(SwitchArm {
                pattern,
                guard,
                value,
            });
            if self.current_punctuator() == Some(Punctuator::Comma) {
                self.bump();
            } else {
                break;
            }
            if self.position == before {
                self.bump();
            }
        }
        let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
        let span = Span::new(governing.span.start, end);
        Expr::new(
            ExprKind::SwitchExpression {
                governing: Box::new(governing),
                keyword: operator,
                arms,
            },
            span,
        )
    }

    /// Whether a bare `_` stands here as a whole pattern -- not `_.M`, `_[0]` or `_ x`, each of
    /// which is a name being used as one.
    fn at_discard_pattern(&self, position: PatternPosition) -> bool {
        matches!(&self.current().kind, TokenKind::Identifier(name) if &**name == "_")
            && !self.at_pattern_designator_at(self.position + 1, position)
            && !matches!(
                self.tokens.get(self.position + 1).map(|token| &token.kind),
                Some(TokenKind::Punctuator(
                    Punctuator::Dot | Punctuator::OpenBracket | Punctuator::LessThan
                ))
            )
    }

    /// Whether the token at the cursor is a pattern's DESIGNATOR -- the `s` of `o is string s`.
    fn at_pattern_designator(&self, position: PatternPosition) -> bool {
        self.at_pattern_designator_at(self.position, position)
    }

    /// [`Self::at_pattern_designator`] at an arbitrary token index.
    ///
    /// **`when` IS THE ONE IDENTIFIER THAT IS USUALLY NOT A DESIGNATOR, AND ONLY IN A SWITCH
    /// ARM.** `x switch { _ when x > 5 => 42 }` is a discard with a GUARD; reading `when` as the
    /// designator of a declaration pattern whose type is `_` consumes it, and the arm then fails at
    /// the `=>` that is no longer where the parser expects one -- fourteen cascading diagnostics
    /// for a legal program, which is how this was found.
    ///
    /// **AFTER AN `is` THERE IS NO GUARD, so `when` is an ordinary identifier there** -- `o is
    /// string when` declares a variable called `when` and must keep doing so. Hence the position.
    ///
    /// The `=>` lookahead keeps the designator reading available for the one arm that wants it:
    /// `o switch { string when => .. }` declares `when`, because a guard cannot be followed by
    /// `=>` with nothing between.
    fn at_pattern_designator_at(&self, index: usize, position: PatternPosition) -> bool {
        let Some(TokenKind::Identifier(name)) = self.tokens.get(index).map(|token| &token.kind)
        else {
            return false;
        };
        if position == PatternPosition::SwitchArm && &**name == "when" {
            return matches!(
                self.tokens.get(index + 1).map(|token| &token.kind),
                Some(TokenKind::Punctuator(Punctuator::EqualsGreaterThan))
            );
        }
        true
    }

    /// A CONSTANT pattern's expression, parsed one precedence level TIGHTER than relational.
    ///
    /// The bound matters in both directions. Looser and `x is 3 && y` takes the `&& y` into the
    /// pattern; tighter and `case 1 + 2` -- a constant expression the language admits -- stops
    /// after the `1`. Shift and below is the level that admits every arithmetic constant and no
    /// operator that could continue the ENCLOSING expression.
    fn parse_constant_pattern(&mut self, operator: Span) -> Pattern {
        let at = Span::empty_at(self.current().span.start);
        let before = self.diagnostics.len();
        let value = self.parse_binary(SHIFT_PRECEDENCE);
        if matches!(value.kind, ExprKind::Error) {
            self.diagnostics.truncate(before);
            self.report(DiagnosticKind::PatternMissing, at);
            return Pattern::Constant(Box::new(value));
        }
        self.gate_feature(Feature::ConstantPattern, operator);
        Pattern::Constant(Box::new(value))
    }

    /// The type on the right of an `is` or `as`, with the nullable suffix rule that position needs.
    ///
    /// THE `?` AFTER THE TARGET IS TWO DIFFERENT TOKENS AND ONLY THE TOKEN AFTER THE NEXT ONE
    /// TELLS THEM APART:
    ///
    /// ```text
    /// x is int ? a : b     the CONDITIONAL operator, C# 1.0
    /// x is int? n          a NULLABLE pattern type, which is CS8116
    /// ```
    ///
    /// Both are `?` IDENT. What differs is the `:` that a conditional must have, so the test is
    /// three tokens deep. Taking the nullable reading is what lets the BINDER give csc's message
    /// instead of the two parse errors the conditional reading collapses into.
    fn parse_is_target_type(&mut self) -> TypeRef {
        let ty = self.parse_type_inner(false);
        if self.current_punctuator() == Some(Punctuator::Question) && self.at_nullable_pattern_type()
        {
            return self.parse_nullable_suffix(ty, true);
        }
        ty
    }

    /// Whether the token at the cursor could begin a TYPE -- a predefined type keyword, a name, or
    /// the `global::` qualifier. Everything else (a literal, a sign, a parenthesis) is a constant
    /// pattern's first token and needs no speculation to say so.
    fn at_type_start(&self) -> bool {
        matches!(self.current().kind, TokenKind::Identifier(_))
            || predefined_type(&self.current().kind).is_some()
    }

    /// Unary expressions (14.6): a prefix operator, a cast, or a postfix chain.
    fn parse_unary(&mut self) -> Expr {
        if self.await_operator_here() {
            let await_span = self.current().span;
            self.bump();
            if !self.in_async_method {
                self.gate_feature(Feature::AsyncFunction, await_span);
                self.report(DiagnosticKind::AwaitOutsideAsync, await_span);
            }
            if self.in_async_method && self.in_unsafe_block {
                self.report(DiagnosticKind::AwaitInUnsafe, await_span);
            }
            let operand = self.parse_unary();
            let span = Span::new(await_span.start, operand.span.end);
            return Expr::new(ExprKind::Await(Box::new(operand)), span);
        }
        if self.current_punctuator() == Some(Punctuator::Asterisk) {
            let start = self.current().span.start;
            self.bump();
            let operand = self.parse_unary();
            let span = Span::new(start, operand.span.end);
            return Expr::new(ExprKind::Dereference(Box::new(operand)), span);
        }
        if self.current_punctuator() == Some(Punctuator::Ampersand) {
            let start = self.current().span.start;
            self.bump();
            let operand = self.parse_unary();
            let span = Span::new(start, operand.span.end);
            return Expr::new(ExprKind::AddressOf(Box::new(operand)), span);
        }
        if let Some(operator) = self.current_punctuator().and_then(prefix_operator) {
            let start = self.current().span.start;
            self.bump();
            let operand = self.parse_unary();
            let span = Span::new(start, operand.span.end);
            return Expr::new(
                ExprKind::Unary {
                    operator,
                    operand: Box::new(operand),
                },
                span,
            );
        }
        if self.current_punctuator() == Some(Punctuator::OpenParen) {
            if let Some(cast) = self.try_parse_cast() {
                return cast;
            }
        }
        self.parse_postfix()
    }

    /// Attempts to parse a cast `( type ) operand` at the current `(`, applying
    /// the disambiguation of 14.6.6: the parenthesized tokens must form a type,
    /// and either that type cannot also be an expression (a predefined or array
    /// type) or the token after the `)` can begin a unary operand. Otherwise this
    /// is a parenthesized expression: the speculative parse is rolled back (its
    /// position and any diagnostics it emitted) and `None` is returned.
    fn try_parse_cast(&mut self) -> Option<Expr> {
        let start = self.current().span.start;
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        self.bump();
        let target = self.parse_type();
        let is_type = !matches!(target.kind, TypeRefKind::Error);
        if is_type && self.current_punctuator() == Some(Punctuator::CloseParen) {
            self.bump();
            let forces_cast = matches!(
                target.kind,
                TypeRefKind::Predefined(_) | TypeRefKind::Array { .. }
            );
            if forces_cast || self.current_begins_cast_operand() {
                let operand = self.parse_unary();
                let span = Span::new(start, operand.span.end);
                return Some(Expr::new(
                    ExprKind::Cast {
                        target,
                        operand: Box::new(operand),
                    },
                    span,
                ));
            }
        }
        self.position = saved_position;
        self.diagnostics.truncate(saved_diagnostics);
        None
    }

    /// Whether the current token can begin the operand of a cast (14.6.6): `~`,
    /// `!`, `(`, an identifier, a literal, or any keyword other than `as`/`is`.
    fn current_begins_cast_operand(&self) -> bool {
        match &self.current().kind {
            TokenKind::Identifier(_)
            | TokenKind::IntegerLiteral { .. }
            | TokenKind::RealLiteral { .. }
            | TokenKind::DecimalLiteral { .. }
            | TokenKind::CharacterLiteral(_)
            | TokenKind::StringLiteral(_) => true,
            TokenKind::Punctuator(punctuator) => matches!(
                punctuator,
                Punctuator::Tilde | Punctuator::Exclamation | Punctuator::OpenParen
            ),
            TokenKind::Keyword(keyword) => !matches!(keyword, Keyword::As | Keyword::Is),
            _ => false,
        }
    }

    /// A primary expression followed by any run of postfix suffixes: member
    /// access, invocation, element access, and postfix `++`/`--` (14.5).
    fn parse_postfix(&mut self) -> Expr {
        let primary = self.parse_primary();
        self.parse_postfix_from(primary, false)
    }

    /// The postfix chain applied to an already-parsed operand.
    ///
    /// **SPLIT FROM [`parse_postfix`](Self::parse_postfix) SO A `?.` CAN TAKE THE REST OF THE CHAIN
    /// AS A SUBTREE.** `a?.B.C` skips `B` AND `C` when `a` is null, so the null-conditional node
    /// has to CONTAIN the trailing accesses rather than be contained by them -- and "the trailing
    /// accesses" is exactly what one more turn of this loop parses. Recursing on a placeholder is
    /// what makes the two readings the same code.
    /// `dependent_only` stops the chain before a postfix `++`/`--`, which is what a null-conditional
    /// access's chain admits: the grammar lets `?.` be followed by `.name`, `(args)` and `[args]`
    /// and NOTHING else. **`b?.N++` is `(b?.N)++`, not `b?.(N++)`** -- measured, csc reports
    /// `CS1059` because the operand is then an `int?` VALUE rather than a variable. Letting the
    /// chain swallow the `++` accepted that program instead.
    fn parse_postfix_from(&mut self, operand: Expr, dependent_only: bool) -> Expr {
        let mut expr = operand;
        loop {
            match self.current_punctuator() {
                Some(Punctuator::Dot) => {
                    self.bump();
                    let (name, end) = self.expect_identifier();
                    let span = Span::new(expr.span.start, end);
                    expr = Expr::new(
                        ExprKind::MemberAccess {
                            receiver: Box::new(expr),
                            name,
                        },
                        span,
                    );
                }
                Some(Punctuator::Arrow) => {
                    let receiver_span = expr.span;
                    self.bump();
                    let (name, end) = self.expect_identifier();
                    let deref = Expr::new(ExprKind::Dereference(Box::new(expr)), receiver_span);
                    let span = Span::new(receiver_span.start, end);
                    expr = Expr::new(
                        ExprKind::MemberAccess {
                            receiver: Box::new(deref),
                            name,
                        },
                        span,
                    );
                }
                Some(Punctuator::OpenParen) => {
                    self.bump();
                    let (arguments, end) = self.parse_arguments(
                        Punctuator::CloseParen,
                        DiagnosticKind::CloseParenExpected,
                    );
                    let span = Span::new(expr.span.start, end);
                    expr = Expr::new(
                        ExprKind::Invocation {
                            receiver: Box::new(expr),
                            type_arguments: Vec::new(),
                            arguments,
                        },
                        span,
                    );
                }
                Some(Punctuator::LessThan) if self.generic_type_name_ahead() => {
                    let (type_arguments, end, _) = self.parse_type_argument_list(false);
                    let span = Span::new(expr.span.start, end);
                    expr = Expr::new(
                        ExprKind::ConstructedType {
                            name: Box::new(expr),
                            type_arguments,
                        },
                        span,
                    );
                }
                Some(Punctuator::LessThan) if self.generic_call_ahead() => {
                    let (type_arguments, _, _) = self.parse_type_argument_list(false);
                    let open = self.current().span.start;
                    self.expect(Punctuator::OpenParen, DiagnosticKind::TokenExpected {
                        expected: "(",
                    });
                    let _ = open;
                    let (arguments, end) = self.parse_arguments(
                        Punctuator::CloseParen,
                        DiagnosticKind::CloseParenExpected,
                    );
                    let span = Span::new(expr.span.start, end);
                    expr = Expr::new(
                        ExprKind::Invocation {
                            receiver: Box::new(expr),
                            type_arguments,
                            arguments,
                        },
                        span,
                    );
                }
                Some(Punctuator::OpenBracket) => {
                    self.bump();
                    let empty = self.current_punctuator() == Some(Punctuator::CloseBracket);
                    let at = self.current().span.start;
                    let (arguments, end) = self.parse_arguments(
                        Punctuator::CloseBracket,
                        DiagnosticKind::TokenExpected { expected: "]" },
                    );
                    if empty {
                        self.report(
                            DiagnosticKind::ValueExpectedInElementAccess,
                            Span::empty_at(at),
                        );
                    }
                    let span = Span::new(expr.span.start, end);
                    expr = Expr::new(
                        ExprKind::ElementAccess {
                            receiver: Box::new(expr),
                            arguments,
                        },
                        span,
                    );
                }
                Some(Punctuator::Question)
                    if matches!(
                        self.tokens.get(self.position + 1).map(|token| &token.kind),
                        Some(TokenKind::Punctuator(
                            Punctuator::Dot | Punctuator::OpenBracket
                        ))
                    ) =>
                {
                    let at = self.current().span.start;
                    self.gate_feature(Feature::NullConditional, Span::empty_at(at));
                    self.bump();
                    let placeholder =
                        Expr::new(ExprKind::ConditionalReceiver, Span::empty_at(at));
                    let access = self.parse_postfix_from(placeholder, true);
                    let span = Span::new(expr.span.start, access.span.end);
                    expr = Expr::new(
                        ExprKind::ConditionalAccess {
                            receiver: Box::new(expr),
                            access: Box::new(access),
                        },
                        span,
                    );
                }
                Some(Punctuator::Exclamation) => {
                    let at = self.current().span;
                    self.gate_feature(Feature::NullForgivingOperator, at);
                    self.bump();
                    while self.current_punctuator() == Some(Punctuator::Exclamation) {
                        self.report(
                            DiagnosticKind::DuplicateNullSuppression,
                            self.current().span,
                        );
                        self.bump();
                    }
                    expr = Expr::new(expr.kind, Span::new(expr.span.start, at.end));
                }
                Some(Punctuator::PlusPlus) if !dependent_only => {
                    expr = self.finish_postfix(expr, PostfixOperator::Increment);
                }
                Some(Punctuator::MinusMinus) if !dependent_only => {
                    expr = self.finish_postfix(expr, PostfixOperator::Decrement);
                }
                _ => break,
            }
        }
        expr
    }

    /// Wraps `operand` in a postfix `++`/`--`, consuming the operator.
    fn finish_postfix(&mut self, operand: Expr, operator: PostfixOperator) -> Expr {
        let end = self.current().span.end;
        self.bump();
        let span = Span::new(operand.span.start, end);
        Expr::new(
            ExprKind::PostfixUnary {
                operator,
                operand: Box::new(operand),
            },
            span,
        )
    }

    /// A primary expression (14.5): a literal, a simple name, `this`, or a
    /// parenthesized expression. A token that can begin none of these is
    /// `CS1525`, recovered with an [`ExprKind::Error`] placeholder.

    /// Turns a scanned interpolated string into its AST parts, parsing each hole's tokens as an
    /// expression IN THIS PARSER'S CONTEXT -- the dialect, and whether we are inside an `async`
    /// method, both of which change how a hole's own tokens read (`await x` is an operator in one
    /// and an identifier in the other).
    fn parse_interpolation_parts(
        &mut self,
        string: &crate::token::InterpolatedString,
    ) -> Vec<InterpolationPart> {
        string
            .parts
            .iter()
            .map(|part| match part {
                crate::token::InterpolatedPart::Literal(units) => {
                    InterpolationPart::Literal(units.clone())
                }
                crate::token::InterpolatedPart::Hole(hole) => InterpolationPart::Hole {
                    expression: Box::new(self.parse_interpolation_operand(&hole.tokens, hole.span)),
                    alignment: if hole.alignment.is_empty() {
                        None
                    } else {
                        Some(Box::new(
                            self.parse_interpolation_operand(&hole.alignment, hole.span),
                        ))
                    },
                    format: hole.format.clone(),
                },
            })
            .collect()
    }

    /// Parses one hole's (or alignment's) tokens as a single expression.
    ///
    /// **THE TOKENS ARRIVE ALREADY SCANNED, WITH THEIR ORIGINAL FILE SPANS**, so a diagnostic
    /// raised in here points at the source the reader wrote. An `EndOfFile` is appended because
    /// every path in this parser expects one and none of them may run off the end.
    ///
    /// An empty hole parses to nothing rather than to `CS1525`: the scanner already reported
    /// `CS1733` for it, and a second complaint about the same absence is a cascade csc does not
    /// emit.
    fn parse_interpolation_operand(&mut self, tokens: &[Token], hole: Span) -> Expr {
        if !tokens.iter().any(|token| !token.is_trivia()) {
            return Expr::new(ExprKind::Error, hole);
        }
        let mut inner = Parser {
            tokens: tokens
                .iter()
                .filter(|token| !token.is_trivia())
                .cloned()
                .chain(core::iter::once(Token::new(
                    TokenKind::EndOfFile,
                    Span::empty_at(hole.end),
                )))
                .collect(),
            position: 0,
            diagnostics: Vec::new(),
            defined_symbols: BTreeSet::new(),
            version: self.version,
            in_async_method: self.in_async_method,
            in_ref_returning_member: self.in_ref_returning_member,
            in_unsafe_block: self.in_unsafe_block,
        };
        let expression = inner.parse_expression();
        if !matches!(inner.current().kind, TokenKind::EndOfFile) {
            let at = inner.current().span;
            inner.diagnostics.push(Diagnostic::new(
                DiagnosticKind::InterpolationCloseDelimiterExpected,
                at,
            ));
        }
        self.diagnostics.append(&mut inner.diagnostics);
        expression
    }

    fn parse_primary(&mut self) -> Expr {
        let span = self.current().span;
        let kind = self.current().kind.clone();
        match kind {
            TokenKind::IntegerLiteral { value, suffix } => {
                self.bump();
                Expr::new(ExprKind::Literal(Literal::Integer { value, suffix }), span)
            }
            TokenKind::RealLiteral { bits, suffix } => {
                self.bump();
                Expr::new(ExprKind::Literal(Literal::Real { bits, suffix }), span)
            }
            TokenKind::DecimalLiteral {
                lo,
                mid,
                hi,
                scale,
            } => {
                self.bump();
                Expr::new(
                    ExprKind::Literal(Literal::Decimal {
                        lo,
                        mid,
                        hi,
                        scale,
                        negative: false,
                    }),
                    span,
                )
            }
            TokenKind::CharacterLiteral(unit) => {
                self.bump();
                Expr::new(ExprKind::Literal(Literal::Character(unit)), span)
            }
            TokenKind::StringLiteral(units) => {
                self.bump();
                Expr::new(ExprKind::Literal(Literal::String(units)), span)
            }
            TokenKind::InterpolatedString(string) => {
                self.bump();
                let parts = self.parse_interpolation_parts(&string);
                Expr::new(ExprKind::InterpolatedString(parts), span)
            }
            TokenKind::Keyword(Keyword::True) => {
                self.bump();
                Expr::new(ExprKind::Literal(Literal::Boolean(true)), span)
            }
            TokenKind::Keyword(Keyword::False) => {
                self.bump();
                Expr::new(ExprKind::Literal(Literal::Boolean(false)), span)
            }
            TokenKind::Keyword(Keyword::Null) => {
                self.bump();
                Expr::new(ExprKind::Literal(Literal::Null), span)
            }
            TokenKind::Keyword(Keyword::This) => {
                self.bump();
                Expr::new(ExprKind::This, span)
            }
            TokenKind::Keyword(Keyword::Base) => {
                self.bump();
                Expr::new(ExprKind::Base, span)
            }
            TokenKind::Keyword(Keyword::New) => self.parse_new(span.start),
            TokenKind::Keyword(Keyword::Typeof) => {
                self.bump();
                self.expect(
                    Punctuator::OpenParen,
                    DiagnosticKind::TokenExpected { expected: "(" },
                );
                let target = self.parse_typeof_operand();
                let end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
                Expr::new(ExprKind::TypeOf(target), Span::new(span.start, end))
            }
            TokenKind::Keyword(Keyword::Sizeof) => {
                self.bump();
                self.expect(
                    Punctuator::OpenParen,
                    DiagnosticKind::TokenExpected { expected: "(" },
                );
                let target = self.parse_type();
                let end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
                Expr::new(ExprKind::SizeOf(target), Span::new(span.start, end))
            }
            TokenKind::Keyword(Keyword::Default) => {
                self.bump();
                self.expect(
                    Punctuator::OpenParen,
                    DiagnosticKind::TokenExpected { expected: "(" },
                );
                let target = self.parse_type();
                let end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
                Expr::new(ExprKind::DefaultValue(target), Span::new(span.start, end))
            }
            TokenKind::Keyword(Keyword::Stackalloc) => {
                self.bump();
                let element = self.parse_non_array_type();
                self.expect(
                    Punctuator::OpenBracket,
                    DiagnosticKind::TokenExpected { expected: "[" },
                );
                let count = self.parse_expression();
                let end = self.expect(
                    Punctuator::CloseBracket,
                    DiagnosticKind::TokenExpected { expected: "]" },
                );
                Expr::new(
                    ExprKind::StackAlloc {
                        element,
                        count: Box::new(count),
                    },
                    Span::new(span.start, end),
                )
            }
            TokenKind::Keyword(Keyword::Checked) => {
                self.bump();
                let (inner, end) = self.parse_parenthesized_operand();
                Expr::new(
                    ExprKind::Checked(Box::new(inner)),
                    Span::new(span.start, end),
                )
            }
            TokenKind::Keyword(Keyword::Unchecked) => {
                self.bump();
                let (inner, end) = self.parse_parenthesized_operand();
                Expr::new(
                    ExprKind::Unchecked(Box::new(inner)),
                    Span::new(span.start, end),
                )
            }
            TokenKind::TypedRefKeyword(TypedRefKeyword::MakeRef) => {
                self.bump();
                let (inner, end) = self.parse_parenthesized_operand();
                Expr::new(
                    ExprKind::MakeRef(Box::new(inner)),
                    Span::new(span.start, end),
                )
            }
            TokenKind::TypedRefKeyword(TypedRefKeyword::RefType) => {
                self.bump();
                let (inner, end) = self.parse_parenthesized_operand();
                Expr::new(
                    ExprKind::RefType(Box::new(inner)),
                    Span::new(span.start, end),
                )
            }
            TokenKind::TypedRefKeyword(TypedRefKeyword::RefValue) => {
                self.bump();
                self.expect(
                    Punctuator::OpenParen,
                    DiagnosticKind::TokenExpected { expected: "(" },
                );
                let reference = self.parse_expression();
                self.expect(
                    Punctuator::Comma,
                    DiagnosticKind::TokenExpected { expected: "," },
                );
                let target = self.parse_type();
                let end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
                Expr::new(
                    ExprKind::RefValue {
                        reference: Box::new(reference),
                        target,
                    },
                    Span::new(span.start, end),
                )
            }
            TokenKind::TypedRefKeyword(TypedRefKeyword::ArgList) => {
                self.bump();
                if self.current_punctuator() == Some(Punctuator::OpenParen) {
                    self.bump();
                    let (arguments, end) = self.parse_arguments(
                        Punctuator::CloseParen,
                        DiagnosticKind::CloseParenExpected,
                    );
                    Expr::new(ExprKind::ArgListCall(arguments), Span::new(span.start, end))
                } else {
                    Expr::new(ExprKind::ArgListHandle, span)
                }
            }
            TokenKind::Identifier(name) => {
                let verbatim = self.current().verbatim;
                self.bump();
                Expr::new(ExprKind::Name { name, verbatim }, span)
            }
            TokenKind::Punctuator(Punctuator::OpenParen) => {
                self.bump();
                let first_name = self.take_tuple_element_name();
                let inner = self.parse_expression();
                if first_name.is_none() && self.current_punctuator() != Some(Punctuator::Comma) {
                    let end =
                        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
                    return Expr::new(
                        ExprKind::Parenthesized(Box::new(inner)),
                        Span::new(span.start, end),
                    );
                }
                self.gate_feature(Feature::Tuples, Span::empty_at(span.start));
                let mut elements = alloc::vec![TupleElementExpr {
                    name: first_name,
                    value: inner,
                }];
                while self.eat(Punctuator::Comma) {
                    let name = self.take_tuple_element_name();
                    let value = self.parse_expression();
                    elements.push(TupleElementExpr { name, value });
                }
                let end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
                if elements.len() < 2 {
                    self.report(
                        DiagnosticKind::TupleTooFewElements,
                        Span::empty_at(end.saturating_sub(1)),
                    );
                }
                Expr::new(ExprKind::Tuple { elements }, Span::new(span.start, end))
            }
            TokenKind::Keyword(keyword)
                if predefined_type(&TokenKind::Keyword(keyword)).is_some() =>
            {
                self.bump();
                let predefined = predefined_type(&TokenKind::Keyword(keyword))
                    .expect("the guard checked it is a predefined type");
                Expr::new(ExprKind::PredefinedType(predefined), span)
            }
            TokenKind::Keyword(Keyword::Delegate) => {
                self.gate_feature(Feature::AnonymousMethods, Span::empty_at(span.start));
                self.bump();
                if self.current_punctuator() == Some(Punctuator::OpenParen) {
                    self.skip_balanced(Punctuator::OpenParen, Punctuator::CloseParen);
                }
                let end = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                    self.skip_balanced(Punctuator::OpenBrace, Punctuator::CloseBrace)
                } else {
                    span.end
                };
                Expr::new(ExprKind::Error, Span::new(span.start, end))
            }
            _ => {
                self.report(DiagnosticKind::ExpressionExpected, span);
                Expr::new(ExprKind::Error, Span::empty_at(span.start))
            }
        }
    }

    /// The `name:` of a tuple element, when the next two tokens are one.
    ///
    /// Shared by the first element and the rest so the rule is written once; the lookahead is the
    /// same one a named argument uses, and for the same reason -- a bare `:` after an identifier
    /// belongs to no other production here.
    fn take_tuple_element_name(&mut self) -> Option<ArgumentName> {
        if !matches!(self.current().kind, TokenKind::Identifier(_)) || !self.next_is(Punctuator::Colon)
        {
            return None;
        }
        let TokenKind::Identifier(text) = &self.current().kind else {
            unreachable!("the guard admitted only an identifier")
        };
        let name = ArgumentName {
            text: text.clone(),
            span: self.current().span,
        };
        self.bump();
        self.bump();
        Some(name)
    }

    /// Parses a `,`-separated argument list up to and including `close` (14.4.1),
    /// returning the arguments and the offset just past the closing bracket.
    fn parse_arguments(&mut self, close: Punctuator, missing: DiagnosticKind) -> (Vec<Argument>, u32) {
        self.parse_argument_list(close, missing, true)
    }

    /// The size list of an array creation `new int[e, ...]` (17.6), which is punctuated like an
    /// argument list and is not one.
    ///
    /// **A NAME IS NOT PART OF THIS GRAMMAR AT ANY VERSION, SO THIS LIST DOES NOT LOOK FOR ONE.**
    /// Because it does not, `new int[n: 3]` stops at the `:` with the missing-separator
    /// diagnostic, which is what csc reports there and at the same column (measured: CS1003, at
    /// the colon). Recognizing the name here and rejecting it afterwards would mean inventing a
    /// diagnostic csc has no equivalent of.
    fn parse_sizes(&mut self, close: Punctuator, missing: DiagnosticKind) -> (Vec<Expr>, u32) {
        let (sizes, end) = self.parse_argument_list(close, missing, false);
        (sizes.into_iter().map(|size| size.value).collect(), end)
    }

    /// The shared body of both: `names` says whether `name :` is part of the grammar being parsed.
    fn parse_argument_list(
        &mut self,
        close: Punctuator,
        missing: DiagnosticKind,
        names: bool,
    ) -> (Vec<Argument>, u32) {
        let mut arguments = Vec::new();
        if self.current_punctuator() == Some(close) {
            let end = self.current().span.end;
            self.bump();
            return (arguments, end);
        }
        loop {
            let before = self.position;
            let mut name = None;
            if names
                && matches!(self.current().kind, TokenKind::Identifier(_))
                && self.next_is(Punctuator::Colon)
            {
                let TokenKind::Identifier(text) = &self.current().kind else {
                    unreachable!("the match above admitted only an identifier")
                };
                let text = text.clone();
                let span = self.current().span;
                self.gate_feature(Feature::NamedArguments, Span::empty_at(span.start));
                self.bump();
                self.bump();
                name = Some(ArgumentName { text, span });
            }
            let ref_out = match self.current_keyword() {
                Some(Keyword::Ref) => {
                    self.bump();
                    Some(false)
                }
                Some(Keyword::Out) => {
                    self.bump();
                    Some(true)
                }
                _ => None,
            };
            let argument = match ref_out {
                Some(true) => self
                    .parse_out_variable_declaration()
                    .unwrap_or_else(|| self.parse_expression()),
                _ => self.parse_expression(),
            };
            let argument = match ref_out {
                Some(out) => {
                    let span = argument.span;
                    Expr::new(
                        ExprKind::RefArgument {
                            position: RefPosition::Argument,
                            out,
                            operand: Box::new(argument),
                        },
                        span,
                    )
                }
                None => argument,
            };
            if arguments.iter().any(|held: &Argument| held.name.is_some())
                && name.is_none()
                && self.version < LanguageVersion::CSharp7_2
            {
                self.report(
                    DiagnosticKind::NonTrailingNamedArgument,
                    Span::empty_at(argument.span.start),
                );
            }
            arguments.push(Argument {
                name,
                value: argument,
            });
            if self.eat(Punctuator::Comma) {
                continue;
            }
            if self.current_punctuator() == Some(close) {
                break;
            }
            if self.position == before {
                break;
            }
            if matches!(
                self.current().kind,
                TokenKind::EndOfFile
                    | TokenKind::Punctuator(
                        Punctuator::CloseParen
                            | Punctuator::CloseBrace
                            | Punctuator::CloseBracket
                            | Punctuator::Semicolon
                    )
            ) {
                break;
            }
            self.report(
                DiagnosticKind::TokenExpected { expected: "," },
                Span::empty_at(self.current().span.start),
            );
        }
        let end = self.expect(close, missing);
        (arguments, end)
    }

    /// Requires an identifier, returning its text and the offset just past it.
    /// A missing identifier is `CS1001`, recovered with an empty name.
    fn expect_identifier(&mut self) -> (Box<str>, u32) {
        if let TokenKind::Identifier(name) = &self.current().kind {
            let name = name.clone();
            let end = self.current().span.end;
            self.bump();
            (name, end)
        } else {
            let at = self.current().span.start;
            self.report(DiagnosticKind::IdentifierExpected, Span::empty_at(at));
            (Box::from(""), at)
        }
    }

    /// [`Parser::expect_identifier`] for a DECLARED name: a local declarator, a parameter, a
    /// `foreach` iteration variable, a `catch` variable. Inside an async method a declared name
    /// spelled `await` without the verbatim prefix is CS4003 (12.8.8.1; code and text measured),
    /// and `@await` passes -- which is why the check needs the token's verbatim flag rather than
    /// its text. One rule, one function, four callers, so a fifth declaration form grows the
    /// check by calling this instead of re-spelling it.
    fn expect_declared_name(&mut self) -> (Box<str>, u32) {
        let span = self.current().span;
        let verbatim = self.current().verbatim;
        let (name, end) = self.expect_identifier();
        if self.in_async_method && !verbatim && &*name == "await" {
            self.report(DiagnosticKind::AwaitAsIdentifier, span);
        }
        (name, end)
    }

    /// Parses a parenthesized operand `( expression )`, shared by `checked` and
    /// `unchecked`. Returns the inner expression and the offset past the `)`.
    fn parse_parenthesized_operand(&mut self) -> (Expr, u32) {
        self.expect(
            Punctuator::OpenParen,
            DiagnosticKind::TokenExpected { expected: "(" },
        );
        let inner = self.parse_expression();
        let end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        (inner, end)
    }

    /// Parses a type (clause 11): a predefined type or a type name, then any
    /// array rank-specifiers (12.1). A missing type is `CS1031`.
    /// Parses a non-array type (11.1): a predefined type or a type name, with no
    /// rank-specifiers. This is the element type for `new` and the base of a
    /// full type. A missing type is `CS1031`.
    /// A TUPLE TYPE `(T, T)` / `(T name, T name)` (C# 7.0), with the scanner on the `(`.
    ///
    /// **A TYPE POSITION IS THE ONE PLACE `(` IS UNAMBIGUOUS**, which is why this needs no
    /// speculation while the EXPRESSION side does: nothing else in the grammar of a type opens
    /// with a bracket, so a `(` here can only start a tuple.
    ///
    /// A one-element list is not a tuple and never becomes one -- see [`TypeRefKind::Tuple`] --
    /// and csc refuses it (CS8124, measured), so the arity is checked here rather than left for a
    /// later pass to discover.
    fn parse_tuple_type(&mut self) -> TypeRef {
        let start = self.current().span.start;
        self.gate_feature(Feature::Tuples, Span::empty_at(start));
        self.bump();
        let mut elements = Vec::new();
        loop {
            let ty = self.parse_type();
            let name = match &self.current().kind {
                TokenKind::Identifier(text) => {
                    let name = ArgumentName {
                        text: text.clone(),
                        span: self.current().span,
                    };
                    self.bump();
                    Some(name)
                }
                _ => None,
            };
            elements.push(TupleElement { name, ty });
            if self.eat(Punctuator::Comma) {
                continue;
            }
            break;
        }
        let end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        if elements.len() < 2 {
            self.report(
                DiagnosticKind::TupleTooFewElements,
                Span::empty_at(end.saturating_sub(1)),
            );
        }
        TypeRef::new(TypeRefKind::Tuple(elements), Span::new(start, end))
    }

    fn parse_non_array_type(&mut self) -> TypeRef {
        if self.current_punctuator() == Some(Punctuator::OpenParen) {
            return self.parse_tuple_type();
        }
        if let Some(predefined) = predefined_type(&self.current().kind) {
            let span = self.current().span;
            self.bump();
            TypeRef::new(TypeRefKind::Predefined(predefined), span)
        } else if matches!(self.current().kind, TokenKind::Identifier(_)) {
            self.parse_type_name()
        } else {
            let at = self.current().span.start;
            self.report(DiagnosticKind::TypeExpected, Span::empty_at(at));
            TypeRef::new(TypeRefKind::Error, Span::empty_at(at))
        }
    }

    fn parse_type(&mut self) -> TypeRef {
        self.parse_type_inner(true)
    }

    /// Parses a type (clause 11). `allow_nullable` gates the C# 2.0 nullable suffix `T?`: it is on
    /// for ordinary type positions (declarations, casts, `typeof`) and OFF for the `is`/`as` target,
    /// where a trailing `?` is the conditional operator (`x is int ? a : b`) not a nullable type.
    fn parse_type_inner(&mut self, allow_nullable: bool) -> TypeRef {
        let base = self.parse_non_array_type();
        self.parse_type_suffixes_inner(base, allow_nullable)
    }

    /// The pointer, nullable and array suffixes that follow an already-parsed base type, in an
    /// ordinary type position (nullable permitted). Split out so a type ARGUMENT can take them
    /// too without re-entering [`parse_non_array_type`](Self::parse_non_array_type), which would
    /// re-read the name.
    fn parse_type_suffixes(&mut self, base: TypeRef) -> TypeRef {
        self.parse_type_suffixes_inner(base, true)
    }

    /// Takes a `?` suffix on an already-parsed type, making it a NULLABLE VALUE TYPE `T?`
    /// (C# 2.0, 11.4), or returns `base` unchanged when there is none. `allow_nullable` is off in
    /// the `is`/`as` target, where a trailing `?` is the conditional operator (`x is int ? a : b`
    /// is a valid C# 1.0 conditional and must stay one) -- that is its whole job.
    ///
    /// **ABOVE C# 2 THE `?` IS TAKEN AFTER ANY TYPE, AND THE AMBIGUITY IS THE SPECULATION'S TO
    /// RESOLVE RATHER THAN THIS FUNCTION'S.** `a ? b : c;` in statement position parses `a?` as a
    /// type and `b` as a declarator, then meets the `:` and fails -- and a failed local declaration
    /// rewinds to the expression reading, which is the machinery
    /// `parse_declaration_or_expression_statement` already runs for `a<b>c`. Restricting the suffix
    /// to predefined keywords would leave `TimeSpan?` and `GpioController?` unwritable, which is
    /// most of what real source spells.
    ///
    /// BELOW C# 2 ONLY A PREDEFINED VALUE-TYPE KEYWORD IS TAKEN, and it is refused: `int ?` is
    /// never a valid C# 1.0 conditional (a keyword is not an expression), whereas a user type
    /// `Foo ?` IS one. CS8022 is the code csc uses, and the `?` is dropped to recover to the
    /// underlying type.
    ///
    /// Separate from the suffix loop so ARRAY CREATION can take it too: `new int?[2]` parses its
    /// element type with [`parse_non_array_type`](Self::parse_non_array_type) and dispatches on the
    /// `[` itself, so without this it read `new int?` and expected an argument list.
    fn parse_nullable_suffix(&mut self, base: TypeRef, allow_nullable: bool) -> TypeRef {
        if !allow_nullable || self.current_punctuator() != Some(Punctuator::Question) {
            return base;
        }
        let start = base.span.start;
        if self.version.supports(Feature::NullableValueTypes) {
            let end = self.current().span.end;
            self.bump();
            return TypeRef::new(TypeRefKind::Nullable(Box::new(base)), Span::new(start, end));
        }
        if matches!(&base.kind, TypeRefKind::Predefined(p) if is_predefined_value_type(*p)) {
            let at = self.current().span.start;
            self.gate_feature(Feature::NullableValueTypes, Span::empty_at(at));
            self.bump();
        }
        base
    }

    fn parse_type_suffixes_inner(&mut self, base: TypeRef, allow_nullable: bool) -> TypeRef {
        let mut base = base;
        let start = base.span.start;
        while !matches!(base.kind, TypeRefKind::Error)
            && self.current_punctuator() == Some(Punctuator::Asterisk)
        {
            let end = self.current().span.end;
            self.bump();
            base = TypeRef::new(TypeRefKind::Pointer(Box::new(base)), Span::new(start, end));
        }
        base = self.parse_nullable_suffix(base, allow_nullable);
        let mut ranks = Vec::new();
        while let Some(rank) = self.try_rank_specifier() {
            ranks.push(rank);
        }
        let Some(&(_, overall_end)) = ranks.last() else {
            return base;
        };
        let mut ty = base;
        for &(rank, _) in ranks.iter().rev() {
            ty = TypeRef::new(
                TypeRefKind::Array {
                    element: Box::new(ty),
                    rank,
                },
                Span::new(start, overall_end),
            );
        }
        ty
    }

    /// Parses a `new` expression (14.5.10): object/delegate creation
    /// `new type ( arguments )` or array creation `new element[lengths] ranks`.
    /// Array initializers (`{ ... }`) are not parsed yet.
    fn parse_new(&mut self, start: u32) -> Expr {
        self.bump();
        if self.current_punctuator() == Some(Punctuator::OpenBrace) {
            self.gate_feature(Feature::AnonymousObjectCreation, Span::empty_at(start));
            let end = self.skip_balanced(Punctuator::OpenBrace, Punctuator::CloseBrace);
            return Expr::new(ExprKind::Error, Span::new(start, end));
        }
        if self.current_punctuator() == Some(Punctuator::OpenParen) {
            let saved_position = self.position;
            let saved_diagnostics = self.diagnostics.len();
            let speculative = self.parse_tuple_type();
            if self.current_punctuator() == Some(Punctuator::OpenBracket)
                && !matches!(speculative.kind, TypeRefKind::Error)
            {
                return self.parse_array_creation(start, speculative);
            }
            self.position = saved_position;
            self.diagnostics.truncate(saved_diagnostics);
        }
        if self.current_punctuator() == Some(Punctuator::OpenParen) {
            self.gate_feature(Feature::TargetTypedNew, Span::empty_at(start));
            self.bump();
            let (arguments, mut end) =
                self.parse_arguments(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
            let mut initializer = None;
            if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                self.gate_initializer_if_unsupported();
                let (parsed, parsed_end) = self.parse_initializer();
                initializer = Some(parsed);
                end = parsed_end;
            }
            return Expr::new(
                ExprKind::ObjectCreation {
                    target: None,
                    arguments,
                    initializer,
                },
                Span::new(start, end),
            );
        }
        let element = self.parse_non_array_type();
        let element = self.parse_nullable_suffix(element, true);
        match self.current_punctuator() {
            Some(Punctuator::OpenParen) => {
                self.bump();
                let (arguments, mut end) = self
                    .parse_arguments(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
                let mut initializer = None;
                if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                    self.gate_initializer_if_unsupported();
                    let (parsed, parsed_end) = self.parse_initializer();
                    initializer = Some(parsed);
                    end = parsed_end;
                }
                Expr::new(
                    ExprKind::ObjectCreation {
                        target: Some(element),
                        arguments,
                        initializer,
                    },
                    Span::new(start, end),
                )
            }
            Some(Punctuator::OpenBracket) => self.parse_array_creation(start, element),
            Some(Punctuator::OpenBrace) => {
                self.gate_initializer_if_unsupported();
                let (initializer, end) = self.parse_initializer();
                Expr::new(
                    ExprKind::ObjectCreation {
                        target: Some(element),
                        arguments: Vec::new(),
                        initializer: Some(initializer),
                    },
                    Span::new(start, end),
                )
            }
            _ => {
                let end = self.expect(
                    Punctuator::OpenParen,
                    DiagnosticKind::TokenExpected { expected: "(" },
                );
                Expr::new(
                    ExprKind::ObjectCreation {
                        target: Some(element),
                        arguments: Vec::new(),
                        initializer: None,
                    },
                    Span::new(start, end),
                )
            }
        }
    }

    /// Reports the C# 3.0 initializer gate at the current `{` when the dialect does not permit one.
    ///
    ///
    /// **The two forms are the SAME version and DIFFERENT nouns**, so which one the message names
    /// is decided here rather than by the feature table: csc reports `'object initializer'` for
    /// `new C { F = 1 }` and `'collection initializer'` for `new ArrayList { 1, 2 }`. Both
    /// measured, as is the tie-break -- **an EMPTY `new C { }` is an object initializer**.
    fn gate_initializer_if_unsupported(&mut self) {
        let feature = if self.initializer_assigns_a_member() {
            Feature::ObjectInitializer
        } else {
            Feature::CollectionInitializer
        };
        self.gate_feature_here(feature);
    }

    /// Parses an object or collection initializer `{ ... }`, positioned on the `{` (C# 3.0).
    ///
    /// **Which kind it is is decided by the first element and nothing else** -- `identifier =` opens
    /// an object initializer, anything else a collection one, and an EMPTY `{ }` is an object
    /// initializer because that is what csc calls it. Both spellings are the same version, so the
    /// distinction exists for the MESSAGE and for what the binder must then check: a collection
    /// initializer needs `IEnumerable` and an `Add` (csc CS1922 / CS1061), an object initializer
    /// needs the named members to exist and be settable (CS0117 / CS0191 / CS0200 / CS1914).
    ///
    /// A TRAILING COMMA is legal in both, measured.
    fn parse_initializer(&mut self) -> (Initializer, u32) {
        let assigns_member = self.initializer_assigns_a_member();
        self.bump();
        let mut members = Vec::new();
        let mut elements = Vec::new();
        while self.current_punctuator() != Some(Punctuator::CloseBrace)
            && !matches!(self.current().kind, TokenKind::EndOfFile)
        {
            let before = self.position;
            if assigns_member {
                members.push(self.parse_member_initializer());
            } else {
                elements.push(self.parse_collection_element());
            }
            if self.position == before {
                self.bump();
            }
            if !self.eat(Punctuator::Comma) {
                break;
            }
        }
        let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
        let initializer = if assigns_member {
            Initializer::Object(members)
        } else {
            Initializer::Collection(elements)
        };
        (initializer, end)
    }

    /// Parses one `name = value` of an object initializer.
    ///
    /// The value is itself an initializer when it is written `{ ... }` -- and that form assigns INTO
    /// the member's existing object rather than constructing one, which is why it is a distinct
    /// [`MemberInitializerValue`] rather than a synthesized `new`.
    /// One element of a collection initializer: an expression, or `{ a, b }` -- a BRACED ARGUMENT
    /// LIST handed to a single `Add` call.
    ///
    /// **THE BRACED FORM IS NOT A NESTED INITIALIZER.** `new D { { 1, "a" } }` is `D.Add(1, "a")`,
    /// which is the whole of how a dictionary initializer works: there is no dictionary construct
    /// in the language, only an element with two arguments. Measured against csc, which takes one,
    /// two or three, and mixes braced and bare elements in one initializer.
    ///
    /// Arity is not checked here. A braced element of two against a one-argument `Add` is
    /// `CS1501`, which is ordinary overload resolution and so the binder's.
    fn parse_collection_element(&mut self) -> CollectionElement {
        let start = self.current().span.start;
        if self.current_punctuator() != Some(Punctuator::OpenBrace) {
            let expression = self.parse_expression();
            let span = expression.span;
            return CollectionElement {
                arguments: alloc::vec![expression],
                span,
            };
        }
        self.bump();
        let mut arguments = Vec::new();
        while self.current_punctuator() != Some(Punctuator::CloseBrace)
            && !matches!(self.current().kind, TokenKind::EndOfFile)
        {
            let before = self.position;
            arguments.push(self.parse_expression());
            if self.position == before {
                self.bump();
            }
            if !self.eat(Punctuator::Comma) {
                break;
            }
        }
        let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
        CollectionElement {
            arguments,
            span: Span::new(start, end),
        }
    }

    fn parse_member_initializer(&mut self) -> MemberInitializer {
        let start = self.current().span.start;
        let (name, _) = self.expect_identifier();
        self.expect(Punctuator::Equals, DiagnosticKind::TokenExpected { expected: "=" });
        let (value, end) = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
            let (nested, nested_end) = self.parse_initializer();
            (MemberInitializerValue::Nested(nested), nested_end)
        } else {
            let expression = self.parse_expression();
            let end = expression.span.end;
            (MemberInitializerValue::Expression(expression), end)
        };
        MemberInitializer {
            name,
            value,
            span: Span::new(start, end),
        }
    }

    /// Whether the initializer starting at the current `{` assigns a member -- `{ F = ... }` -- as
    /// opposed to listing elements. csc's own discriminator, and it needs only the two tokens after
    /// the brace: an object initializer's first element is always `identifier =`.
    ///
    /// An empty `{ }` answers `true`, because that is what csc calls it. `{ F = { G = 1 } }` also
    /// answers `true` from the outer brace and again from the inner one, matching csc's two
    /// diagnostics for that program.
    fn initializer_assigns_a_member(&self) -> bool {
        if matches!(
            self.tokens.get(self.position + 1).map(|token| &token.kind),
            Some(TokenKind::Punctuator(Punctuator::CloseBrace))
        ) {
            return true;
        }
        matches!(
            self.tokens.get(self.position + 1).map(|token| &token.kind),
            Some(TokenKind::Identifier(_))
        ) && matches!(
            self.tokens.get(self.position + 2).map(|token| &token.kind),
            Some(TokenKind::Punctuator(Punctuator::Equals))
        )
    }

    /// Parses the bracket part of an array creation, with the scanner at the
    /// first `[`. The first dimension is either size expressions (`[e, ...]`) or
    /// an unsized rank-specifier (`[]`/`[,]`, whose initializer is deferred);
    /// trailing rank-specifiers give the jagged dimensions.
    fn parse_array_creation(&mut self, start: u32, element: TypeRef) -> Expr {
        let (lengths, rank, mut end) = if let Some((rank, end)) = self.try_rank_specifier() {
            (Vec::new(), rank, end)
        } else {
            self.bump();
            let (sizes, end) = self.parse_sizes(
                Punctuator::CloseBracket,
                DiagnosticKind::TokenExpected { expected: "]" },
            );
            let rank = (sizes.len() as u8).max(1);
            (sizes, rank, end)
        };
        let mut extra_ranks = Vec::new();
        while let Some((extra, extra_end)) = self.try_rank_specifier() {
            extra_ranks.push(extra);
            end = extra_end;
        }
        let initializer = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
            let initializer = self.parse_array_initializer();
            end = initializer.span.end;
            Some(Box::new(initializer))
        } else {
            None
        };
        Expr::new(
            ExprKind::ArrayCreation {
                element,
                lengths,
                rank,
                extra_ranks,
                initializer,
            },
            Span::new(start, end),
        )
    }

    /// Parses an array initializer `{ variable-initializer-list? ,? }` (14.5.10.2),
    /// the scanner at the `{`. Each element is a nested array initializer or an
    /// expression; a trailing comma is allowed.
    fn parse_array_initializer(&mut self) -> Expr {
        let start = self.current().span.start;
        self.bump();
        let mut elements = Vec::new();
        loop {
            if self.current_punctuator() == Some(Punctuator::CloseBrace)
                || matches!(self.current().kind, TokenKind::EndOfFile)
            {
                break;
            }
            let element = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                self.parse_array_initializer()
            } else {
                self.parse_expression()
            };
            elements.push(element);
            if !self.eat(Punctuator::Comma) {
                break;
            }
        }
        let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
        Expr::new(ExprKind::ArrayInitializer(elements), Span::new(start, end))
    }

    /// Parses the operand of `typeof` (ECMA-334 4th ed 14.5.11): a `type` as everywhere else, or
    /// the `unbound-type-name` that is legal in this one position and no other.
    ///
    /// **THE TWO GRAMMARS OVERLAP AND THE SPEC BREAKS THE TIE, SO THE UNBOUND FORM IS TRIED
    /// FIRST AND ONLY COMMITS ON ITS OWN SHAPE.** 14.5.11: *when the operand is a sequence of
    /// tokens that satisfies the grammars of both unbound-type-name and type-name, namely when it
    /// contains neither a generic-dimension-specifier nor a type-argument-list, the sequence of
    /// tokens is considered to be a type-name.* A `generic-dimension-specifier` is the only thing
    /// that tells them apart, so [`Parser::parse_unbound_type_name`] answers `None` unless it finds
    /// one -- `typeof(int)`, `typeof(T)`, `typeof(List<int>)` and `typeof(void)` all fall through
    /// to the ordinary type parse, unchanged.
    ///
    /// The grammar puts `unbound-type-name` in this production alone, so the parser does too: no
    /// other type position accepts a `generic-dimension-specifier`, and none of them needs a rule
    /// of its own to refuse one.
    fn parse_typeof_operand(&mut self) -> TypeRef {
        match self.parse_unbound_type_name() {
            Some(unbound) => unbound,
            None => self.parse_type(),
        }
    }

    /// Parses an `unbound-type-name` (14.5.11) at the cursor, or answers `None` having consumed
    /// nothing.
    ///
    /// The shape is `identifier ('.' identifier)* '<' ','* '>'`, and the trailing
    /// `generic-dimension-specifier` is what makes it one: `arity` is the comma count plus one, so
    /// `<>` is 1 and `<,>` is 2.
    ///
    /// **SPECULATIVE, BECAUSE THE DECIDING TOKEN IS PAST THE NAME.** `List<>` and `List<int>` are
    /// identical until the token after the `<`, and a dotted name puts arbitrarily many tokens in
    /// front of that. So this walks the shape and rewinds the position AND the diagnostics on any
    /// mismatch, exactly as [`type_argument_list_ahead`](Self::type_argument_list_ahead) does --
    /// the same pair, for the same reason: a half-walked name that reported `IdentifierExpected` on
    /// the way must not leave that behind for a `typeof(int[])` the caller then parses cleanly.
    ///
    /// Below C# 2 this answers `None` rather than reporting, so the refusal comes from the ordinary
    /// type parse -- one feature diagnostic (CS8022) at the `<`, the same one `typeof(List<int>)`
    /// draws, from the same line of code.
    ///
    /// A specifier on a NON-FINAL part -- `typeof(List<>.Enumerator)` -- is not accepted, though
    /// the grammar allows one after every part and csc compiles it. [`TypeRefKind::Unbound`]
    /// carries ONE arity for a whole dotted name, so there is nowhere to record which part the
    /// specifier sat on. This answers `None` for it and the ordinary type parse refuses.
    fn parse_unbound_type_name(&mut self) -> Option<TypeRef> {
        if !self.version.supports(Feature::Generics) {
            return None;
        }
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        let unbound = self.walk_unbound_type_name();
        if unbound.is_none() || self.diagnostics.len() != saved_diagnostics {
            self.position = saved_position;
            self.diagnostics.truncate(saved_diagnostics);
            return None;
        }
        unbound
    }

    /// The walk behind [`parse_unbound_type_name`](Self::parse_unbound_type_name), which owns the
    /// rewind. Consumes tokens as it goes and answers `None` the moment the shape does not hold.
    fn walk_unbound_type_name(&mut self) -> Option<TypeRef> {
        let start = self.current().span.start;
        let mut parts = Vec::new();
        loop {
            match &self.current().kind {
                TokenKind::Identifier(name) => {
                    parts.push(name.clone());
                    self.bump();
                }
                _ => return None,
            }
            if self.current_punctuator() != Some(Punctuator::Dot) {
                break;
            }
            self.bump();
        }
        if self.current_punctuator() != Some(Punctuator::LessThan) {
            return None;
        }
        self.bump();
        let mut arity = 1;
        while self.current_punctuator() == Some(Punctuator::Comma) {
            self.bump();
            arity += 1;
        }
        if self.current_punctuator() != Some(Punctuator::GreaterThan) {
            return None;
        }
        let end = self.current().span.end;
        self.bump();
        Some(TypeRef::new(
            TypeRefKind::Unbound { parts, arity },
            Span::new(start, end),
        ))
    }

    /// Parses a type name (11.1): `identifier ('.' identifier)*`, with a generic type-argument
    /// list `<...>` where one follows and the dialect permits it (25.5).
    fn parse_type_name(&mut self) -> TypeRef {
        self.parse_type_name_inner(false).0
    }

    /// [`parse_type_name`](Self::parse_type_name), also returning the right-angle brackets its own
    /// argument list closed on and did not need -- see [`AngleCredit`]. `nested` says whether an
    /// enclosing type-argument list is waiting for a `>`.
    ///
    /// **EVERY PART MAY CARRY A TYPE-ARGUMENT LIST, AND THAT IS 10.8's OWN SHAPE**:
    /// `namespace-or-type-name` is `identifier type-argument-list_opt` repeated over the dots. So
    /// `List<int>.Enumerator` and `Box<int>.Pair<string>` are read here in full, rather than
    /// stopping at the first `>` and leaving a `.` that no declarator can follow (CS1002).
    fn parse_type_name_inner(&mut self, nested: bool) -> (TypeRef, AngleCredit) {
        let start = self.current().span.start;
        let verbatim = self.current().verbatim;
        let (mut name, mut end) = self.expect_identifier();
        let mut parts: Vec<TypeNamePart> = Vec::new();
        let mut constructed = false;
        let mut credit = AngleCredit::default();
        loop {
            let mut arguments = Vec::new();
            if self.current_punctuator() == Some(Punctuator::LessThan) {
                if self.version.supports(Feature::Generics) {
                    let (list, list_end, list_credit) = self.parse_type_argument_list(nested);
                    arguments = list;
                    end = list_end;
                    credit = list_credit;
                    constructed = true;
                } else {
                    let at = self.current().span.start;
                    self.gate_feature(Feature::Generics, Span::empty_at(at));
                    end = self.skip_type_argument_list();
                }
            }
            parts.push(TypeNamePart { name, arguments });
            if credit.closes > 0 {
                break;
            }
            if self.current_punctuator() != Some(Punctuator::Dot) {
                break;
            }
            self.bump();
            let (next, next_end) = self.expect_identifier();
            name = next;
            end = next_end;
        }
        let kind = if constructed {
            TypeRefKind::Generic { parts }
        } else {
            TypeRefKind::Name(parts.into_iter().map(|part| part.name).collect())
        };
        (
            TypeRef::new(kind, Span::new(start, end)).with_verbatim_name(verbatim),
            credit,
        )
    }

    /// Parses a generic type-argument list from the current `<` (25.5): `< type (, type)* >`.
    /// Returns the arguments, the offset past the closing `>`, and any [`AngleCredit`] left for an
    /// enclosing list.
    ///
    /// **THE CLOSING `>` IS NOT ALWAYS A `>` TOKEN, AND THE OBVIOUS FIX IS A MISCOMPILE.** The
    /// lexer takes the longest match (9.4.5), so `List<List<int>>` ends in ONE
    /// [`Punctuator::GreaterThanGreaterThan`] closing two levels. Narrowing that token in place --
    /// rewriting it as the second `>` -- reads as the natural fix and is wrong: a local declaration
    /// is told from an expression by parsing a type SPECULATIVELY and rolling the position back
    /// ([`parse_declaration_or_expression_statement`](Self::parse_declaration_or_expression_statement)),
    /// and a rolled-back position does not restore a rewritten token. **Measured by building it:**
    /// `a<b>>c;` parsed as `a<b>`, rolled back, and re-parsed as `(a < b) > c` -- **one `>` of the
    /// source silently deleted from the program**, compiling clean, at every dialect above C# 2.
    ///
    /// So the `>>` is consumed WHOLE by the inner list, which hands its second half outward as a
    /// credit. Nothing in the token stream is mutated, and rollback stays position-only.
    ///
    /// **ONLY A `nested` LIST MAY CONSUME A `>>`, AND THAT IS C#'s RULE RATHER THAN AN
    /// IMPLEMENTATION CONVENIENCE.** The grammar splits `>>` into two `>` for a type-argument list
    /// and nowhere else, so an OUTERMOST list has nothing to give the second half to. Letting it
    /// close on one anyway reads `a<b>>c;` as a local declaration `a<b> c;` where the language --
    /// and csc -- read the shift expression `a < b >> c`. Refusing to close leaves the `>>` in
    /// place, the speculative type parse fails, and the statement falls through to the expression
    /// reading it should have had.
    /// Whether the `<` at the cursor opens a generic method call's type-argument list rather than a
    /// less-than operator -- decided by SPECULATION, because the two are identical up to the `>`.
    ///
    /// `a < b > (c)` is two comparisons and `M<int>(x)` is one call, and no amount of looking at the
    /// `<` itself separates them. So this parses a candidate type-argument list and asks what
    /// follows: a `(` means the list was real. That is the invocation case of the spec's
    /// disambiguation rule -- ECMA-334 9.2.3 admits `)`, `]`, `:`, `;`, `,`, `.`, `?`, `==` and `!=` too
    /// as well, and none of those is reachable from the postfix loop, so admitting them here would
    /// claim `<` for a type-argument list in positions this parser then could not use.
    ///
    /// **THE RESTORE MUST TRUNCATE DIAGNOSTICS, NOT ONLY THE POSITION.** A failed speculation
    /// walks `a < b > (c)` as a type-argument list, and `b > (c)` reports errors on the way. Rewinding
    /// the cursor while leaving those behind turns a legal comparison into a program that refuses to
    /// compile, with diagnostics pointing at a construct the user did not write. Every other
    /// speculation in this parser restores both; this is the same pair.
    /// Whether the `<` at the cursor opens a type-argument list closing on a `.` -- a CONSTRUCTED
    /// TYPE whose static member is being accessed, `Box<int>.Count`.
    ///
    /// Every condition [`Parser::generic_call_ahead`] states applies here for the same reasons; only
    /// the FOLLOWER differs. Kept as its own predicate rather than a parameter because the two
    /// commit to different nodes, and a shared one would have to hand back which follower matched.
    fn generic_type_name_ahead(&mut self) -> bool {
        self.type_argument_list_ahead(Punctuator::Dot)
    }

    /// Whether a member declaration's name begins an EXPLICIT INTERFACE IMPLEMENTATION's qualifier
    /// (20.4.1) -- `IFace.Member`, or the constructed form `IEnumerable<int>.GetEnumerator`.
    ///
    /// The bare case is one token of lookahead. The constructed case cannot be, because a `<` after
    /// an identifier is ambiguous with a comparison and only the follower resolves it; so it defers
    /// to [`Parser::generic_type_name_ahead`], which speculates a type-argument list closing on a
    /// `.` -- exactly this shape. That predicate is FALSE for `void I.M<T>()`, whose list closes on
    /// a `(`, so the generic-METHOD path this one sits above cannot be captured by it.
    fn explicit_interface_qualifier_ahead(&mut self) -> bool {
        if !matches!(self.current().kind, TokenKind::Identifier(_)) {
            return false;
        }
        if self.next_is(Punctuator::Dot) {
            return true;
        }
        if !self.next_is(Punctuator::LessThan) {
            return false;
        }
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        self.bump();
        let committed = self.generic_type_name_ahead();
        self.position = saved_position;
        self.diagnostics.truncate(saved_diagnostics);
        committed
    }

    fn generic_call_ahead(&mut self) -> bool {
        self.type_argument_list_ahead(Punctuator::OpenParen)
    }

    /// The shared speculation behind [`Parser::generic_call_ahead`] and
    /// [`Parser::generic_type_name_ahead`]: parse a type-argument list at the cursor, decide whether
    /// it IS one from `follower`, and rewind either way.
    fn type_argument_list_ahead(&mut self, follower: Punctuator) -> bool {
        if !self.version.supports(Feature::Generics) {
            return false;
        }
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        let (arguments, _, credit) = self.parse_type_argument_list(false);
        let clean = self.diagnostics.len() == saved_diagnostics;
        let committed = clean
            && credit.closes == 0
            && !arguments.is_empty()
            && self.current_punctuator() == Some(follower);
        self.position = saved_position;
        self.diagnostics.truncate(saved_diagnostics);
        committed
    }

    fn parse_type_argument_list(&mut self, nested: bool) -> (Vec<TypeRef>, u32, AngleCredit) {
        self.bump();
        let mut arguments = Vec::new();
        let mut credit = AngleCredit::default();
        loop {
            let (argument, argument_credit) = self.parse_type_argument();
            arguments.push(argument);
            if argument_credit.closes > 0 {
                credit = argument_credit;
            }
            if !self.eat(Punctuator::Comma) {
                break;
            }
        }
        if credit.closes > 0 {
            credit.closes -= 1;
            return (arguments, credit.end, credit);
        }
        match self.current_punctuator() {
            Some(Punctuator::GreaterThanGreaterThan) if nested => {
                let end = self.current().span.end;
                self.bump();
                (arguments, end, AngleCredit { closes: 1, end })
            }
            _ => {
                let end = self.expect(
                    Punctuator::GreaterThan,
                    DiagnosticKind::TokenExpected { expected: ">" },
                );
                (arguments, end, AngleCredit::default())
            }
        }
    }

    /// Parses one type argument, propagating any [`AngleCredit`] its own nested argument list left.
    ///
    /// A type argument is a full type (25.5.1): `List<int[]>`, `List<int?>`, `List<C.D<int>>` are
    /// all legal, so this is [`parse_type_inner`](Self::parse_type_inner)'s work with the credit
    /// carried out. Array and pointer suffixes cannot themselves produce a credit -- only a
    /// type-argument list can -- so it is only the base type's credit that travels.
    fn parse_type_argument(&mut self) -> (TypeRef, AngleCredit) {
        if !matches!(self.current().kind, TokenKind::Identifier(_)) {
            return (self.parse_type(), AngleCredit::default());
        }
        let (base, credit) = self.parse_type_name_inner(true);
        if credit.closes > 0 {
            return (base, credit);
        }
        (self.parse_type_suffixes(base), AngleCredit::default())
    }

    /// A `<` where a DECLARATION's name has just been read begins a type-PARAMETER list
    /// (`class C<T>`, `T M<T>(T)`). Parses it where the dialect permits generics; below C# 2
    /// reports the feature diagnostic once and skips the balanced `<...>` so the rest of the
    /// declaration still parses. Returns the declared parameters, empty in both other cases.
    ///
    /// **THE USE SITE HAD THE GATE AND THE DECLARATION SITES DID NOT, AND THE DIFFERENCE WAS
    /// VISIBLE IN THE DIAGNOSTICS.** Measured against csc at `/langversion:ISO-1`, which answers all
    /// three of `class C<T> { }`, `T M<T>(T x)` and `List<int>` with ONE feature diagnostic. lcsc
    /// answered the use site the same way and the two declaration sites with a parse CASCADE --
    /// `class C<T> { }` drew CS1031 + CS1514 + CS1519 and a generic method drew six codes, none of
    /// which csc emits there. Codes csc does not emit are the false-positive column, so this was
    /// not merely untidy.
    ///
    /// **PARSING IS NOT ACCEPTING, AND THE DIFFERENCE LIVES IN THE BINDER.**
    /// [`crate::version::Feature::Generics`] is in the NOT-IMPLEMENTED set, so above C# 2 the
    /// binder's `gate_feature` refuses the construct as LAM0001 -- *permitted by this dialect, not
    /// built in this compiler* -- which is a different sentence from CS8022 and is deliberately NOT
    /// silenced by raising the language version. Producing the tree here is what lets the binder
    /// name the construct at all; a half-implemented feature that reaches EMIT is the silent
    /// miscompile this pair of bits exists to prevent, and is how the automatically-implemented
    /// -property hole got in.
    fn parse_type_parameter_list(&mut self) -> Vec<TypeParameter> {
        if self.current_punctuator() != Some(Punctuator::LessThan) {
            return Vec::new();
        }
        if !self.version.supports(Feature::Generics) {
            let at = self.current().span.start;
            self.gate_feature(Feature::Generics, Span::empty_at(at));
            self.skip_type_argument_list();
            return Vec::new();
        }
        self.bump();
        let mut parameters = Vec::new();
        loop {
            let span = self.current().span;
            let (name, end) = self.expect_identifier();
            parameters.push(TypeParameter {
                name,
                span: Span::new(span.start, end),
            });
            if !self.eat(Punctuator::Comma) {
                break;
            }
        }
        self.expect(Punctuator::GreaterThan, DiagnosticKind::TokenExpected { expected: ">" });
        parameters
    }

    /// Parses a run of `where` clauses (C# 2.0; ECMA-334 4th ed 25.7), the
    /// *type-parameter-constraints-clauses* that follow a type's base list or a method's parameter
    /// list. Returns empty when the next token does not begin one.
    ///
    /// **`where` IS CONTEXTUAL, so the test is three tokens rather than one.** It is an ordinary
    /// identifier everywhere else, and `class where { }` declares a type called `where` that a
    /// program may then name in a base list -- `class C : where { }`. Requiring
    /// `where` + identifier + `:` is what tells the clause from the type: a base list has already
    /// consumed its `:` by the time this runs, so a bare `where` in that position is a type name and
    /// is left alone.
    ///
    /// **Parsed at every language version, INCLUDING below C# 2 where generics are gated.** The
    /// gate fires once, in [`Parser::parse_type_parameter_list`]; if the clauses were then left
    /// unparsed, `class C<T> where T : class { }` would report the honest CS8022 and follow it with
    /// a cascade at `where` from the brace expectation -- codes csc does not emit here, which is the
    /// false-positive column this parser measures itself against. Producing the tree is not
    /// accepting it; the binder is what refuses.
    fn parse_type_parameter_constraint_clauses(&mut self) -> Vec<TypeParameterConstraintClause> {
        let mut clauses = Vec::new();
        while self.current_is_constraint_clause_start() {
            let start = self.current().span.start;
            self.bump();
            let parameter_span = self.current().span;
            let (parameter, parameter_end) = self.expect_identifier();
            self.expect(Punctuator::Colon, DiagnosticKind::TokenExpected { expected: ":" });
            let mut constraints = Vec::new();
            let end = loop {
                let constraint = self.parse_type_parameter_constraint();
                let at = constraint.span().end;
                constraints.push(constraint);
                if !self.eat(Punctuator::Comma) {
                    break at;
                }
            };
            clauses.push(TypeParameterConstraintClause {
                parameter,
                parameter_span: Span::new(parameter_span.start, parameter_end),
                constraints,
                span: Span::new(start, end),
            });
        }
        clauses
    }

    /// Whether the current position begins a `where` clause: the contextual keyword, then an
    /// identifier, then `:`. See [`Parser::parse_type_parameter_constraint_clauses`] for why all
    /// three are required.
    fn current_is_constraint_clause_start(&self) -> bool {
        if !matches!(self.current_contextual_keyword(), Some("where")) {
            return false;
        }
        matches!(
            self.tokens.get(self.position + 1).map(|token| &token.kind),
            Some(TokenKind::Identifier(_))
        ) && matches!(
            self.tokens.get(self.position + 2).map(|token| &token.kind),
            Some(TokenKind::Punctuator(Punctuator::Colon))
        )
    }

    /// Parses one constraint inside a `where` clause: `class`, `struct`, `new()`, or a type.
    ///
    /// **`class` and `struct` are reserved words here, not type names**, so they are matched as
    /// keywords rather than routed through [`Parser::parse_type`] -- which would take `class` as a
    /// parse error rather than the reference-type constraint. `new` is likewise the constructor
    /// constraint and never an object creation: a constraint position has no expression in it.
    fn parse_type_parameter_constraint(&mut self) -> TypeParameterConstraint {
        let span = self.current().span;
        match &self.current().kind {
            TokenKind::Keyword(Keyword::Class) => {
                self.bump();
                TypeParameterConstraint::ReferenceType(span)
            }
            TokenKind::Keyword(Keyword::Struct) => {
                self.bump();
                TypeParameterConstraint::ValueType(span)
            }
            TokenKind::Keyword(Keyword::New) => {
                self.bump();
                self.expect(Punctuator::OpenParen, DiagnosticKind::TokenExpected { expected: "(" });
                let end =
                    self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
                TypeParameterConstraint::DefaultConstructor(Span::new(span.start, end))
            }
            _ => TypeParameterConstraint::Type(self.parse_type()),
        }
    }

    /// Skips a generic type-argument list `< ... >` from the current `<`, balancing nested `<`/`>`
    /// (a `>>` right-shift token closes two levels). Returns the offset past the last `>`. Recovery
    /// after the CS8022 generics gate so the rest of the declaration still parses.
    fn skip_type_argument_list(&mut self) -> u32 {
        let mut depth = 0i32;
        let mut end = self.current().span.end;
        loop {
            if matches!(self.current().kind, TokenKind::EndOfFile) {
                return end;
            }
            end = self.current().span.end;
            let punct = self.current_punctuator();
            self.bump();
            match punct {
                Some(Punctuator::LessThan) => depth += 1,
                Some(Punctuator::GreaterThan) => {
                    depth -= 1;
                    if depth <= 0 {
                        return end;
                    }
                }
                Some(Punctuator::GreaterThanGreaterThan) => {
                    depth -= 2;
                    if depth <= 0 {
                        return end;
                    }
                }
                _ => {}
            }
        }
    }

    /// Skips a balanced `open` ... `close` group starting at the current `open` token, counting
    /// nested pairs, and returns the offset past the matching `close` (or end-of-file). Recovery
    /// for a gated construct (an anonymous method's body, an object initializer) so the rest of the
    /// enclosing statement still parses. Brace/paren pairs never span a string or comment -- those
    /// are single tokens by the time the parser sees them -- so a token-level count is exact.
    fn skip_balanced(&mut self, open: Punctuator, close: Punctuator) -> u32 {
        let mut depth = 0u32;
        let mut end = self.current().span.end;
        loop {
            if matches!(self.current().kind, TokenKind::EndOfFile) {
                return end;
            }
            end = self.current().span.end;
            let punct = self.current_punctuator();
            self.bump();
            if punct == Some(open) {
                depth += 1;
            } else if punct == Some(close) {
                depth -= 1;
                if depth == 0 {
                    return end;
                }
            }
        }
    }

    /// Consumes an array rank-specifier `[` `,`* `]` if one begins here, returning
    /// its rank and the offset past the `]`. A `[` that is not a rank-specifier
    /// (it holds an index expression) is left untouched for element access.
    fn try_rank_specifier(&mut self) -> Option<(u8, u32)> {
        if self.current_punctuator() != Some(Punctuator::OpenBracket) {
            return None;
        }
        let mut index = self.position + 1;
        let mut commas: u8 = 0;
        while matches!(
            self.tokens.get(index).map(|token| &token.kind),
            Some(TokenKind::Punctuator(Punctuator::Comma))
        ) {
            commas = commas.saturating_add(1);
            index += 1;
        }
        match self.tokens.get(index) {
            Some(token) if token.kind == TokenKind::Punctuator(Punctuator::CloseBracket) => {
                let end = token.span.end;
                self.position = index + 1;
                Some((commas + 1, end))
            }
            _ => None,
        }
    }
}

/// Maps a token to the type-test operation it spells, if any (14.9.9, 14.9.10).
fn type_test_operation(kind: &TokenKind) -> Option<TypeTestOperation> {
    match kind {
        TokenKind::Keyword(Keyword::Is) => Some(TypeTestOperation::Is),
        TokenKind::Keyword(Keyword::As) => Some(TypeTestOperation::As),
        _ => None,
    }
}

/// Maps a token to the predefined type it spells, if any (11.1.4).
fn predefined_type(kind: &TokenKind) -> Option<PredefinedType> {
    let TokenKind::Keyword(keyword) = kind else {
        return None;
    };
    Some(match keyword {
        Keyword::Bool => PredefinedType::Bool,
        Keyword::Byte => PredefinedType::Byte,
        Keyword::Sbyte => PredefinedType::Sbyte,
        Keyword::Short => PredefinedType::Short,
        Keyword::Ushort => PredefinedType::Ushort,
        Keyword::Int => PredefinedType::Int,
        Keyword::Uint => PredefinedType::Uint,
        Keyword::Long => PredefinedType::Long,
        Keyword::Ulong => PredefinedType::Ulong,
        Keyword::Char => PredefinedType::Char,
        Keyword::Float => PredefinedType::Float,
        Keyword::Double => PredefinedType::Double,
        Keyword::Decimal => PredefinedType::Decimal,
        Keyword::String => PredefinedType::String,
        Keyword::Object => PredefinedType::Object,
        Keyword::Void => PredefinedType::Void,
        _ => return None,
    })
}

/// Drops the parser cascade a gated post-1.0 operator provokes, leaving only its feature diagnostic.
/// A gated operator (`=>`, `??`, `::`, `?.`, `?[`) tokenizes as an opaque `Unknown` after the lexer
/// has already reported CS8022 for it; the parser, not recognizing the token, then adds a secondary
/// CS1xxx at the SAME byte offset. csc reports only the feature diagnostic there, so a non-CS8022
/// diagnostic that shares an offset with a CS8022 is the cascade the gate already explains -- remove
/// it. Diagnostics at any OTHER offset are untouched, so a genuinely unexpected character (also
/// `Unknown`, but CS1056 with no CS8022 at its offset) keeps its normal report.
fn without_gated_operator_cascades(mut diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let gated: Vec<u32> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code() == 8022)
        .map(|diagnostic| diagnostic.span.start)
        .collect();
    if gated.is_empty() {
        return diagnostics;
    }
    diagnostics.retain(|diagnostic| diagnostic.code() == 8022 || !gated.contains(&diagnostic.span.start));
    diagnostics
}

/// Whether `ty` is one of the predefined VALUE types -- the ones with a nullable form `T?` (C# 2.0).
/// The predefined reference types (`string`, `object`) and `void` are excluded: `string?`/`object?`
/// are nullable reference types (a separate, later feature) and `void?` is meaningless.
fn is_predefined_value_type(ty: PredefinedType) -> bool {
    matches!(
        ty,
        PredefinedType::Bool
            | PredefinedType::Byte
            | PredefinedType::Sbyte
            | PredefinedType::Short
            | PredefinedType::Ushort
            | PredefinedType::Int
            | PredefinedType::Uint
            | PredefinedType::Long
            | PredefinedType::Ulong
            | PredefinedType::Char
            | PredefinedType::Float
            | PredefinedType::Double
            | PredefinedType::Decimal
    )
}

/// Maps a punctuator to the prefix unary operator it spells, if any (14.6).
fn prefix_operator(punctuator: Punctuator) -> Option<UnaryOperator> {
    Some(match punctuator {
        Punctuator::Plus => UnaryOperator::Plus,
        Punctuator::Minus => UnaryOperator::Minus,
        Punctuator::Exclamation => UnaryOperator::Not,
        Punctuator::Tilde => UnaryOperator::Complement,
        Punctuator::PlusPlus => UnaryOperator::PreIncrement,
        Punctuator::MinusMinus => UnaryOperator::PreDecrement,
        _ => return None,
    })
}

/// Maps a punctuator to its binary operator and precedence, if any. A larger
/// precedence binds tighter (14.7 multiplicative is highest here, 14.12 the
/// conditional-or `||` is lowest).
/// The precedence of `<`, `>`, `<=`, `>=`, `is` and `as` in [`binary_operator`]'s table.
///
/// Named rather than repeated because `is` and `as` are handled OUTSIDE that table -- they take a
/// type or a pattern on the right, not an expression -- and the loop still has to place them at
/// the same level.
const RELATIONAL_PRECEDENCE: u8 = 7;

/// The precedence of `<<` and `>>`: one level TIGHTER than relational, and so the lowest level a
/// constant pattern's expression may reach. See [`Parser::parse_constant_pattern`].
const SHIFT_PRECEDENCE: u8 = 8;

/// The precedence of a SWITCH EXPRESSION's `switch`: one level TIGHTER than multiplicative, the
/// highest entry in [`binary_operator`]'s table, and looser than unary.
///
/// Measured rather than read off the grammar, in both directions: `2 * x switch { _ => "s" }` is
/// `CS0019` for `int * string` under csc, so the switch took `x` and not `2 * x`; and
/// `-x switch { _ => "s" }` compiles, so it took `-x` and not `x`.
const SWITCH_PRECEDENCE: u8 = 11;

/// Which position a pattern is being parsed in, for the two decisions that differ between them.
/// See [`Parser::parse_pattern`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternPosition {
    /// After an `is`, where a bare name -- `_` included -- is C# 1.0's type test.
    TypeTest,
    /// A switch expression's arm, where a bare name is a constant and `_` is the discard.
    SwitchArm,
}

/// Where a pattern's text ends, for the span of the expression that contains it.
fn pattern_end(pattern: &Pattern) -> u32 {
    match pattern {
        Pattern::Type(ty) => ty.span.end,
        Pattern::Constant(value) => value.span.end,
        Pattern::Declaration { span, .. } => span.end,
        Pattern::Discard(at) => at.end,
    }
}

fn binary_operator(punctuator: Punctuator) -> Option<(BinaryOperator, u8)> {
    Some(match punctuator {
        Punctuator::Asterisk => (BinaryOperator::Multiply, 10),
        Punctuator::Slash => (BinaryOperator::Divide, 10),
        Punctuator::Percent => (BinaryOperator::Modulo, 10),
        Punctuator::Plus => (BinaryOperator::Add, 9),
        Punctuator::Minus => (BinaryOperator::Subtract, 9),
        Punctuator::LessThanLessThan => (BinaryOperator::LeftShift, 8),
        Punctuator::GreaterThanGreaterThan => (BinaryOperator::RightShift, 8),
        Punctuator::LessThan => (BinaryOperator::LessThan, 7),
        Punctuator::GreaterThan => (BinaryOperator::GreaterThan, 7),
        Punctuator::LessThanEquals => (BinaryOperator::LessThanOrEqual, 7),
        Punctuator::GreaterThanEquals => (BinaryOperator::GreaterThanOrEqual, 7),
        Punctuator::EqualsEquals => (BinaryOperator::Equal, 6),
        Punctuator::ExclamationEquals => (BinaryOperator::NotEqual, 6),
        Punctuator::Ampersand => (BinaryOperator::BitwiseAnd, 5),
        Punctuator::Caret => (BinaryOperator::BitwiseXor, 4),
        Punctuator::Bar => (BinaryOperator::BitwiseOr, 3),
        Punctuator::AmpersandAmpersand => (BinaryOperator::LogicalAnd, 2),
        Punctuator::BarBar => (BinaryOperator::LogicalOr, 1),
        _ => return None,
    })
}

/// Maps a punctuator to the assignment operator it spells, if any (14.14).
fn assignment_operator(punctuator: Punctuator) -> Option<AssignmentOperator> {
    Some(match punctuator {
        Punctuator::Equals => AssignmentOperator::Assign,
        Punctuator::PlusEquals => AssignmentOperator::Add,
        Punctuator::MinusEquals => AssignmentOperator::Subtract,
        Punctuator::AsteriskEquals => AssignmentOperator::Multiply,
        Punctuator::SlashEquals => AssignmentOperator::Divide,
        Punctuator::PercentEquals => AssignmentOperator::Modulo,
        Punctuator::AmpersandEquals => AssignmentOperator::And,
        Punctuator::BarEquals => AssignmentOperator::Or,
        Punctuator::CaretEquals => AssignmentOperator::Xor,
        Punctuator::LessThanLessThanEquals => AssignmentOperator::LeftShift,
        Punctuator::GreaterThanGreaterThanEquals => AssignmentOperator::RightShift,
        _ => return None,
    })
}

/// The declaration modifier a keyword denotes, if it is one (17.2 and elsewhere).
fn modifier_of(keyword: Keyword) -> Option<Modifier> {
    Some(match keyword {
        Keyword::New => Modifier::New,
        Keyword::Public => Modifier::Public,
        Keyword::Protected => Modifier::Protected,
        Keyword::Internal => Modifier::Internal,
        Keyword::Private => Modifier::Private,
        Keyword::Abstract => Modifier::Abstract,
        Keyword::Sealed => Modifier::Sealed,
        Keyword::Static => Modifier::Static,
        Keyword::Readonly => Modifier::Readonly,
        Keyword::Volatile => Modifier::Volatile,
        Keyword::Virtual => Modifier::Virtual,
        Keyword::Override => Modifier::Override,
        Keyword::Extern => Modifier::Extern,
        Keyword::Const => Modifier::Const,
        Keyword::Unsafe => Modifier::Unsafe,
        _ => return None,
    })
}

/// A type named by its dotted parts, for a synthesized signature.
fn synth_type(parts: &[&str], span: Span) -> TypeRef {
    TypeRef {
        kind: TypeRefKind::Name(parts.iter().map(|part| Box::from(*part)).collect()),
        span,
        verbatim_name: false,
    }
}

/// A predefined type, for a synthesized signature.
fn synth_predefined(which: PredefinedType, span: Span) -> TypeRef {
    TypeRef {
        kind: TypeRefKind::Predefined(which),
        span,
        verbatim_name: false,
    }
}

/// `A.B.Outer<Argument>` -- a constructed type, for a synthesized signature.
///
/// **EVERY GENERATED NAME IS FULLY QUALIFIED, AND THAT IS NOT STYLE.** A desugar is spliced into
/// whatever file declared the record, and that file need not have a `using` for anything the
/// generated members name -- `record R(int X);` in a file with no `using System;` still gets an
/// `IEquatable<R>` base and an `EqualityComparer<T>` in two bodies. An unqualified spelling is
/// `CS0246` there, and worse, a qualified one cannot be captured by a `using` the user happens to
/// write later.
fn synth_generic(namespace: &[&str], name: &str, argument: TypeRef, span: Span) -> TypeRef {
    let mut parts: Vec<TypeNamePart> = namespace
        .iter()
        .map(|part| TypeNamePart {
            name: Box::from(*part),
            arguments: Vec::new(),
        })
        .collect();
    parts.push(TypeNamePart {
        name: Box::from(name),
        arguments: alloc::vec![argument],
    });
    TypeRef {
        kind: TypeRefKind::Generic { parts },
        span,
        verbatim_name: false,
    }
}

/// `EqualityComparer<T>` in EXPRESSION position, for the `.Default` a record's equality and hash
/// both go through.
///
/// A constructed type NAMED as the receiver of a static member is an expression, not a `TypeRef`:
/// the two spellings are different nodes and only this one can carry `.Default` after it.
fn synth_comparer(argument: TypeRef, span: Span) -> Expr {
    let mut receiver = Expr::name(Box::from("System"), span);
    for part in ["Collections", "Generic"] {
        receiver = synth_member(receiver, part, span);
    }
    Expr::new(
        ExprKind::ConstructedType {
            name: Box::new(synth_member(receiver, "EqualityComparer", span)),
            type_arguments: alloc::vec![argument],
        },
        span,
    )
}

/// An ordinary by-value parameter.
fn synth_parameter(ty: TypeRef, name: &str, span: Span) -> Parameter {
    Parameter {
        modifier: None,
        ty,
        name: Box::from(name),
        default_value: None,
        span,
    }
}

/// `receiver.name`.
fn synth_member(receiver: Expr, name: &str, span: Span) -> Expr {
    Expr::new(
        ExprKind::MemberAccess {
            receiver: Box::new(receiver),
            name: Box::from(name),
        },
        span,
    )
}

/// `receiver(arguments)`.
fn synth_call(receiver: Expr, arguments: Vec<Expr>, span: Span) -> Expr {
    Expr::new(
        ExprKind::Invocation {
            receiver: Box::new(receiver),
            type_arguments: Vec::new(),
            arguments: arguments.into_iter().map(Argument::positional).collect(),
        },
        span,
    )
}

/// `left <op> right`.
fn synth_binary(left: Expr, operator: BinaryOperator, right: Expr, span: Span) -> Expr {
    Expr::new(
        ExprKind::Binary {
            operator,
            left: Box::new(left),
            right: Box::new(right),
        },
        span,
    )
}

/// `{ statements }`.
fn synth_block(statements: Vec<Stmt>, span: Span) -> Stmt {
    Stmt::new(StmtKind::Block(statements), span)
}

/// `return value;`
fn synth_return(value: Expr, span: Span) -> Stmt {
    Stmt::new(StmtKind::Return(Some(value)), span)
}

/// `{ return value; }` -- the body every generated member that computes one thing has.
fn synth_return_body(value: Expr, span: Span) -> Stmt {
    synth_block(alloc::vec![synth_return(value, span)], span)
}

/// The MEMBERS a record's value equality, hash and `ToString` range over, in declaration order:
/// its positional parameters, then the public instance fields and properties its body declares.
///
/// **csc RANGES OVER THE STATE, NOT OVER THE POSITIONAL LIST**, which is why the body form gets a
/// full equality group too -- `record R { public int X; }` compares and prints `X`. A `static` or
/// `const` member is not state and is skipped; a private one is not printed. Each entry is
/// (name, declared type), and the type is what picks the `EqualityComparer<T>` instantiation.
fn record_state_members(
    parameters: &[Parameter],
    members: &[Member],
) -> Vec<(Box<str>, TypeRef)> {
    let mut state: Vec<(Box<str>, TypeRef)> = parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), parameter.ty.clone()))
        .collect();
    let instance_public = |modifiers: &[Modifier]| {
        modifiers.iter().any(|m| matches!(m, Modifier::Public))
            && !modifiers
                .iter()
                .any(|m| matches!(m, Modifier::Static | Modifier::Const))
    };
    for member in members {
        match member {
            Member::Field {
                modifiers,
                ty,
                declarators,
                ..
            } if instance_public(modifiers) => {
                for declarator in declarators {
                    if !state.iter().any(|(had, _)| *had == declarator.name) {
                        state.push((declarator.name.clone(), ty.clone()));
                    }
                }
            }
            Member::Property {
                modifiers,
                ty,
                name,
                getter: Some(_),
                ..
            } if instance_public(modifiers) => {
                if !state.iter().any(|(had, _)| had == name) {
                    state.push((name.clone(), ty.clone()));
                }
            }
            _ => {}
        }
    }
    state
}

/// Adds the members a `record` declaration generates, as ORDINARY SYNTAX.
///
/// **A DESUGAR, FOR THE REASON `=>` IS ONE.** Every member csc synthesizes for a record is
/// expressible in the language it is a record of -- a property, a constructor, a method -- so
/// producing them here means the binder, the flow analysis and the emitter see shapes they already
/// handle, and none of them learns what a record is. The one name that cannot be TYPED,
/// `<Clone>$`, is still an ordinary identifier in the tree.
///
/// **EVERY FORM GETS THE WHOLE GROUP, AND NONE OF IT IS OPTIONAL.** A record missing `Equals(R)`
/// or `op_Equality` is an assembly whose members a csc-built consumer calls and does not find, so
/// the positional list decides only the properties, the constructor and `Deconstruct` -- value
/// equality, the hash, `ToString`/`PrintMembers`, `<Clone>$` and the copy constructor are
/// generated for the body form too. Measured over three record forms against csc's own inventory
/// (`tools/record-split.ps1 -Members`).
///
/// **A RECORD THAT INHERITS IS REFUSED RATHER THAN GENERATED**, because the shape differs in ways
/// only the binder can see: a derived record OVERRIDES `EqualityContract`, `ToString`,
/// `PrintMembers` and `GetHashCode` where a base one introduces them, seals `Equals(Base)` beside a
/// new `Equals(Derived)`, and returns the BASE type from `<Clone>$`. Whether a base name IS a
/// record is not answerable here -- this runs in the parser, where a base list is a list of names
/// -- and guessing it wrong emits a second `virtual` slot for a member the base already has, which
/// is a silent dispatch failure rather than a diagnostic.
///
/// A member the SOURCE already declares is left alone: C# lets a record write its own `X` beside
/// `record R(int X)`, and the generated one then does not exist. The same rule covers a
/// hand-written `Equals`, `ToString` or `PrintMembers`.
fn synthesize_record_members(declaration: &mut TypeDecl) {
    let Some(parts) = declaration.record.clone() else {
        return;
    };
    let parameters = parts.parameters.clone().unwrap_or_default();
    let positional = parts.parameters.is_some();
    let span = parts.keyword_span;
    let declares = |name: &str, members: &[Member]| {
        members.iter().any(|member| match member {
            Member::Property { name: existing, .. } => &**existing == name,
            Member::Field { declarators, .. } => {
                declarators.iter().any(|declarator| &*declarator.name == name)
            }
            _ => false,
        })
    };
    let mut generated = Vec::new();
    for parameter in &parameters {
        if declares(&parameter.name, &declaration.members) {
            continue;
        }
        let accessor = |is_init: bool| Accessor {
            attributes: Vec::new(),
            modifiers: Vec::new(),
            body: None,
            is_init,
            span,
        };
        generated.push(Member::Property {
            modifiers: alloc::vec![Modifier::Public],
            ty: parameter.ty.clone(),
            name: parameter.name.clone(),
            name_span: span,
            getter: Some(accessor(false)),
            setter: Some(accessor(true)),
            explicit_interface: None,
            initializer: None,
            attributes: Vec::new(),
            span,
        });
    }
    let body = Stmt::new(
        StmtKind::Block(
            parameters
                .iter()
                .map(|parameter| {
                    let target = Expr::new(
                        ExprKind::MemberAccess {
                            receiver: Box::new(Expr::new(ExprKind::This, span)),
                            name: parameter.name.clone(),
                        },
                        span,
                    );
                    Stmt::new(
                        StmtKind::Expression(Expr::new(
                            ExprKind::Assignment {
                                operator: AssignmentOperator::Assign,
                                target: Box::new(target),
                                value: Box::new(Expr::name(parameter.name.clone(), span)),
                            },
                            span,
                        )),
                        span,
                    )
                })
                .collect(),
        ),
        span,
    );
    if positional {
        generated.push(Member::Constructor {
            modifiers: alloc::vec![Modifier::Public],
            name: declaration.name.clone(),
            parameters: parameters.clone(),
            is_vararg: false,
            initializer: None,
            body,
            header_span: span,
            attributes: Vec::new(),
            span,
        });
    }
    if declaration.bases.is_empty() {
        let equatable =
            synthesize_record_equality(declaration, &parameters, positional, span, &mut generated);
        declaration.members.extend(generated);
        if let Some(equatable) = equatable {
            declaration.bases.push(equatable);
        }
    } else {
        declaration.members.extend(generated);
    }
}

/// The value-equality group, `ToString`/`PrintMembers`, `<Clone>$`, the copy constructor and
/// `Deconstruct` -- every member csc generates for a record beyond its positional properties.
///
/// **THE SHAPE IS csc'S, MEASURED RATHER THAN RECALLED** (`tools/record-split.ps1 -Members`, and
/// the IL of each body read once). `EqualityContract` is `protected virtual`, `Equals(R)` is
/// `public virtual` and NEW, `Equals(object)` and `ToString` and `GetHashCode` are OVERRIDES,
/// `PrintMembers` is `protected virtual`, `<Clone>$` is `public virtual`, and the copy constructor
/// is `protected`. Those modifiers are the whole difference between an assembly a csc-built
/// consumer can call and one it cannot.
fn synthesize_record_equality(
    declaration: &TypeDecl,
    parameters: &[Parameter],
    positional: bool,
    span: Span,
    generated: &mut Vec<Member>,
) -> Option<TypeRef> {
    let state = record_state_members(parameters, &declaration.members);
    let record_ty = TypeRef {
        kind: TypeRefKind::Name(alloc::vec![declaration.name.clone()]),
        span,
        verbatim_name: false,
    };
    let bool_ty = || synth_predefined(PredefinedType::Bool, span);
    let int_ty = || synth_predefined(PredefinedType::Int, span);
    let string_ty = || synth_predefined(PredefinedType::String, span);
    let object_ty = || synth_predefined(PredefinedType::Object, span);
    let this = || Expr::new(ExprKind::This, span);
    let name_expr = |name: &str| Expr::name(Box::from(name), span);
    let declares_method = |name: &str| {
        declaration.members.iter().any(|member| match member {
            Member::Method { name: existing, .. } => &**existing == name,
            Member::Property { name: existing, .. } => &**existing == name,
            _ => false,
        })
    };
    let declares_operator = |wanted: OverloadableOperator| {
        declaration.members.iter().any(|member| {
            matches!(member, Member::Operator { operator, .. } if *operator == wanted)
        })
    };

    if !declares_method("EqualityContract") {
        generated.push(Member::Property {
            modifiers: alloc::vec![Modifier::Protected, Modifier::Virtual],
            ty: synth_type(&["System", "Type"], span),
            name_span: span,
            name: Box::from("EqualityContract"),
            getter: Some(Accessor {
                attributes: Vec::new(),
                modifiers: Vec::new(),
                body: Some(synth_return_body(
                    Expr::new(ExprKind::TypeOf(record_ty.clone()), span),
                    span,
                )),
                is_init: false,
                span,
            }),
            setter: None,
            explicit_interface: None,
            initializer: None,
            attributes: Vec::new(),
            span,
        });
    }

    let already_copies = declaration.members.iter().any(|member| {
        matches!(member, Member::Constructor { parameters, .. }
            if parameters.len() == 1
                && matches!(&parameters[0].ty.kind, TypeRefKind::Name(parts)
                    if parts.len() == 1 && parts[0] == declaration.name))
    });
    if !already_copies {
        let copies = state
            .iter()
            .map(|(name, _)| {
                Stmt::new(
                    StmtKind::Expression(Expr::new(
                        ExprKind::Assignment {
                            operator: AssignmentOperator::Assign,
                            target: Box::new(synth_member(this(), name, span)),
                            value: Box::new(synth_member(name_expr("original"), name, span)),
                        },
                        span,
                    )),
                    span,
                )
            })
            .collect();
        generated.push(Member::Constructor {
            modifiers: alloc::vec![Modifier::Protected],
            name: declaration.name.clone(),
            parameters: alloc::vec![synth_parameter(record_ty.clone(), "original", span)],
            is_vararg: false,
            initializer: None,
            body: synth_block(copies, span),
            header_span: span,
            attributes: Vec::new(),
            span,
        });
    }

    generated.push(Member::Method {
        modifiers: alloc::vec![Modifier::Public, Modifier::Virtual],
        return_type: record_ty.clone(),
        name: Box::from("<Clone>$"),
        name_span: span,
        type_parameters: Vec::new(),
        constraints: Vec::new(),
        parameters: Vec::new(),
        is_vararg: false,
        body: Some(synth_return_body(
            Expr::new(
                ExprKind::ObjectCreation {
                    target: Some(record_ty.clone()),
                    arguments: alloc::vec![Argument::positional(this())],
                    initializer: None,
                },
                span,
            ),
            span,
        )),
        explicit_interface: None,
        attributes: Vec::new(),
        span,
    });

    if !declares_method("Equals") {
        let other = || name_expr("other");
        let mut test = synth_binary(
            Expr::new(
                ExprKind::Cast {
                    target: object_ty(),
                    operand: Box::new(other()),
                },
                span,
            ),
            BinaryOperator::NotEqual,
            Expr::new(ExprKind::Literal(Literal::Null), span),
            span,
        );
        test = synth_binary(
            test,
            BinaryOperator::LogicalAnd,
            synth_binary(
                synth_member(this(), "EqualityContract", span),
                BinaryOperator::Equal,
                synth_member(other(), "EqualityContract", span),
                span,
            ),
            span,
        );
        for (name, ty) in &state {
            let comparer = synth_member(synth_comparer(ty.clone(), span), "Default", span);
            test = synth_binary(
                test,
                BinaryOperator::LogicalAnd,
                synth_call(
                    synth_member(comparer, "Equals", span),
                    alloc::vec![
                        synth_member(this(), name, span),
                        synth_member(other(), name, span),
                    ],
                    span,
                ),
                span,
            );
        }
        generated.push(Member::Method {
            modifiers: alloc::vec![Modifier::Public, Modifier::Virtual],
            return_type: bool_ty(),
            name: Box::from("Equals"),
            name_span: span,
            type_parameters: Vec::new(),
            constraints: Vec::new(),
            parameters: alloc::vec![synth_parameter(record_ty.clone(), "other", span)],
            is_vararg: false,
            body: Some(synth_return_body(test, span)),
            explicit_interface: None,
            attributes: Vec::new(),
            span,
        });
        generated.push(Member::Method {
            modifiers: alloc::vec![Modifier::Public, Modifier::Override],
            return_type: bool_ty(),
            name: Box::from("Equals"),
            name_span: span,
            type_parameters: Vec::new(),
            constraints: Vec::new(),
            parameters: alloc::vec![synth_parameter(object_ty(), "obj", span)],
            is_vararg: false,
            body: Some(synth_return_body(
                synth_call(
                    synth_member(this(), "Equals", span),
                    alloc::vec![Expr::new(
                        ExprKind::TypeTest {
                            operation: TypeTestOperation::As,
                            operand: Box::new(name_expr("obj")),
                            target: Pattern::Type(record_ty.clone()),
                        },
                        span,
                    )],
                    span,
                ),
                span,
            )),
            explicit_interface: None,
            attributes: Vec::new(),
            span,
        });
    }

    if !declares_method("GetHashCode") {
        let hash_of = |receiver: Expr, ty: TypeRef| {
            synth_call(
                synth_member(
                    synth_member(synth_comparer(ty, span), "Default", span),
                    "GetHashCode",
                    span,
                ),
                alloc::vec![receiver],
                span,
            )
        };
        let mut hash = hash_of(
            synth_member(this(), "EqualityContract", span),
            synth_type(&["System", "Type"], span),
        );
        for (name, ty) in &state {
            hash = synth_binary(
                synth_binary(
                    hash,
                    BinaryOperator::Multiply,
                    Expr::new(
                        ExprKind::Unary {
                            operator: UnaryOperator::Minus,
                            operand: Box::new(Expr::new(
                                ExprKind::Literal(Literal::Integer {
                                    value: 1_521_134_295,
                                    suffix: IntegerSuffix::None,
                                }),
                                span,
                            )),
                        },
                        span,
                    ),
                    span,
                ),
                BinaryOperator::Add,
                hash_of(synth_member(this(), name, span), ty.clone()),
                span,
            );
        }
        generated.push(Member::Method {
            modifiers: alloc::vec![Modifier::Public, Modifier::Override],
            return_type: int_ty(),
            name: Box::from("GetHashCode"),
            name_span: span,
            type_parameters: Vec::new(),
            constraints: Vec::new(),
            parameters: Vec::new(),
            is_vararg: false,
            body: Some(synth_return_body(hash, span)),
            explicit_interface: None,
            attributes: Vec::new(),
            span,
        });
    }

    let eq_parameters = || {
        alloc::vec![
            synth_parameter(record_ty.clone(), "left", span),
            synth_parameter(record_ty.clone(), "right", span),
        ]
    };
    let null_of = |which: &str| {
        synth_binary(
            Expr::new(
                ExprKind::Cast {
                    target: object_ty(),
                    operand: Box::new(name_expr(which)),
                },
                span,
            ),
            BinaryOperator::Equal,
            Expr::new(ExprKind::Literal(Literal::Null), span),
            span,
        )
    };
    let equality_test = || {
        Expr::new(
            ExprKind::Conditional {
                condition: Box::new(null_of("left")),
                when_true: Box::new(null_of("right")),
                when_false: Box::new(synth_call(
                    synth_member(name_expr("left"), "Equals", span),
                    alloc::vec![name_expr("right")],
                    span,
                )),
            },
            span,
        )
    };
    if !declares_operator(OverloadableOperator::Equality) {
        generated.push(Member::Operator {
            modifiers: alloc::vec![Modifier::Public, Modifier::Static],
            return_type: bool_ty(),
            operator: OverloadableOperator::Equality,
            parameters: eq_parameters(),
            body: synth_return_body(equality_test(), span),
            attributes: Vec::new(),
            span,
        });
    }
    if !declares_operator(OverloadableOperator::Inequality) {
        generated.push(Member::Operator {
            modifiers: alloc::vec![Modifier::Public, Modifier::Static],
            return_type: bool_ty(),
            operator: OverloadableOperator::Inequality,
            parameters: eq_parameters(),
            body: synth_return_body(
                Expr::new(
                    ExprKind::Unary {
                        operator: UnaryOperator::Not,
                        operand: Box::new(Expr::new(
                            ExprKind::Parenthesized(Box::new(equality_test())),
                            span,
                        )),
                    },
                    span,
                ),
                span,
            ),
            attributes: Vec::new(),
            span,
        });
    }

    let sb_ty = synth_type(&["System", "Text", "StringBuilder"], span);
    if !declares_method("PrintMembers") {
        let builder = || name_expr("builder");
        let append = |value: Expr| {
            Stmt::new(
                StmtKind::Expression(synth_call(
                    synth_member(builder(), "Append", span),
                    alloc::vec![value],
                    span,
                )),
                span,
            )
        };
        let literal = |text: &str| {
            Expr::new(
                ExprKind::Literal(Literal::String(text.encode_utf16().collect())),
                span,
            )
        };
        let mut statements = alloc::vec![Stmt::new(
            StmtKind::Expression(synth_call(
                synth_member(
                    synth_member(
                        synth_member(
                            synth_member(name_expr("System"), "Runtime", span),
                            "CompilerServices",
                            span,
                        ),
                        "RuntimeHelpers",
                        span,
                    ),
                    "EnsureSufficientExecutionStack",
                    span,
                ),
                Vec::new(),
                span,
            )),
            span,
        )];
        for (index, (name, _)) in state.iter().enumerate() {
            if index > 0 {
                statements.push(append(literal(", ")));
            }
            statements.push(append(literal(name)));
            statements.push(append(literal(" = ")));
            statements.push(append(Expr::new(
                ExprKind::Cast {
                    target: object_ty(),
                    operand: Box::new(synth_member(this(), name, span)),
                },
                span,
            )));
        }
        statements.push(synth_return(
            Expr::new(ExprKind::Literal(Literal::Boolean(!state.is_empty())), span),
            span,
        ));
        generated.push(Member::Method {
            modifiers: alloc::vec![Modifier::Protected, Modifier::Virtual],
            return_type: bool_ty(),
            name: Box::from("PrintMembers"),
            name_span: span,
            type_parameters: Vec::new(),
            constraints: Vec::new(),
            parameters: alloc::vec![synth_parameter(sb_ty.clone(), "builder", span)],
            is_vararg: false,
            body: Some(synth_block(statements, span)),
            explicit_interface: None,
            attributes: Vec::new(),
            span,
        });
    }
    if !declares_method("ToString") {
        let builder = || name_expr("builder");
        let literal = |text: &str| {
            Expr::new(
                ExprKind::Literal(Literal::String(text.encode_utf16().collect())),
                span,
            )
        };
        let append = |value: Expr| {
            Stmt::new(
                StmtKind::Expression(synth_call(
                    synth_member(builder(), "Append", span),
                    alloc::vec![value],
                    span,
                )),
                span,
            )
        };
        let mut statements = alloc::vec![Stmt::new(
            StmtKind::LocalDeclaration {
                ty: sb_ty.clone(),
                declarators: alloc::vec![VariableDeclarator {
                    name: Box::from("builder"),
                    initializer: Some(Expr::new(
                        ExprKind::ObjectCreation {
                            target: Some(sb_ty.clone()),
                            arguments: Vec::new(),
                            initializer: None,
                        },
                        span,
                    )),
                    span,
                }],
                is_const: false,
            },
            span,
        )];
        statements.push(append(literal(&declaration.name)));
        statements.push(append(literal(" { ")));
        statements.push(Stmt::new(
            StmtKind::If {
                condition: synth_call(
                    synth_member(this(), "PrintMembers", span),
                    alloc::vec![builder()],
                    span,
                ),
                then_branch: Box::new(synth_block(
                    alloc::vec![append(Expr::new(
                        ExprKind::Literal(Literal::Character(u16::from(b' '))),
                        span,
                    ))],
                    span,
                )),
                else_branch: None,
            },
            span,
        ));
        statements.push(append(Expr::new(
            ExprKind::Literal(Literal::Character(u16::from(b'}'))),
            span,
        )));
        statements.push(synth_return(
            synth_call(synth_member(builder(), "ToString", span), Vec::new(), span),
            span,
        ));
        generated.push(Member::Method {
            modifiers: alloc::vec![Modifier::Public, Modifier::Override],
            return_type: string_ty(),
            name: Box::from("ToString"),
            name_span: span,
            type_parameters: Vec::new(),
            constraints: Vec::new(),
            parameters: Vec::new(),
            is_vararg: false,
            body: Some(synth_block(statements, span)),
            explicit_interface: None,
            attributes: Vec::new(),
            span,
        });
    }

    if positional && !parameters.is_empty() && !declares_method("Deconstruct") {
        let out_parameters = parameters
            .iter()
            .map(|parameter| Parameter {
                modifier: Some(ParameterModifier::Out),
                ty: parameter.ty.clone(),
                name: parameter.name.clone(),
                default_value: None,
                span,
            })
            .collect();
        let stores = parameters
            .iter()
            .map(|parameter| {
                Stmt::new(
                    StmtKind::Expression(Expr::new(
                        ExprKind::Assignment {
                            operator: AssignmentOperator::Assign,
                            target: Box::new(Expr::name(parameter.name.clone(), span)),
                            value: Box::new(synth_member(this(), &parameter.name, span)),
                        },
                        span,
                    )),
                    span,
                )
            })
            .collect();
        generated.push(Member::Method {
            modifiers: alloc::vec![Modifier::Public],
            return_type: synth_predefined(PredefinedType::Void, span),
            name: Box::from("Deconstruct"),
            name_span: span,
            type_parameters: Vec::new(),
            constraints: Vec::new(),
            parameters: out_parameters,
            is_vararg: false,
            body: Some(synth_block(stores, span)),
            explicit_interface: None,
            attributes: Vec::new(),
            span,
        });
    }

    let declares_constructor = declaration
        .members
        .iter()
        .any(|member| matches!(member, Member::Constructor { .. }));
    if !positional && !declares_constructor {
        generated.push(Member::Constructor {
            modifiers: alloc::vec![Modifier::Public],
            name: declaration.name.clone(),
            parameters: Vec::new(),
            is_vararg: false,
            initializer: None,
            body: synth_block(Vec::new(), span),
            header_span: span,
            attributes: Vec::new(),
            span,
        });
    }

    let already_equatable = declaration.bases.iter().any(|base| {
        matches!(&base.kind, TypeRefKind::Generic { parts }
            if parts.len() == 1 && &*parts[0].name == "IEquatable")
    });
    if already_equatable {
        None
    } else {
        Some(synth_generic(&["System"], "IEquatable", record_ty, span))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::String;

    /// Renders an expression as a parenthesized prefix form, so a test can assert
    /// on structure (and thus precedence and associativity) in one readable line.
    fn dump(expr: &Expr) -> String {
        match &expr.kind {
            ExprKind::Tuple { elements } => {
                let mut text = String::from("(tuple");
                for element in elements {
                    text.push(' ');
                    if let Some(name) = &element.name {
                        text.push_str(&name.text);
                        text.push(':');
                    }
                    text.push_str(&dump(&element.value));
                }
                text.push(')');
                text
            }
            ExprKind::Lambda {
                parameters, body, ..
            } => {
                let mut out = String::from("(lambda (");
                for (index, parameter) in parameters.iter().enumerate() {
                    if index > 0 {
                        out.push(' ');
                    }
                    if parameter.ty.is_some() {
                        out.push_str("typed:");
                    }
                    out.push_str(&parameter.name);
                }
                out.push_str(") ");
                match &**body {
                    crate::ast::LambdaBody::Expression(expression) => out.push_str(&dump(expression)),
                    crate::ast::LambdaBody::Block(_) => out.push_str("block"),
                }
                out.push(')');
                out
            }
            ExprKind::Literal(Literal::Integer { value, .. }) => format!("{value}"),
            ExprKind::Literal(Literal::Real { .. }) => String::from("real"),
            ExprKind::Literal(Literal::Decimal { .. }) => String::from("decimal"),
            ExprKind::Literal(Literal::Character(unit)) => format!("char:{unit}"),
            ExprKind::Literal(Literal::String(_)) => String::from("str"),
            ExprKind::Literal(Literal::Boolean(value)) => format!("{value}"),
            ExprKind::Literal(Literal::Null) => String::from("null"),
            ExprKind::Name { name, verbatim } => {
                if *verbatim {
                    format!("@{name}")
                } else {
                    String::from(&**name)
                }
            }
            ExprKind::PredefinedType(predefined) => String::from(predefined_text(*predefined)),
            ExprKind::ConditionalAccess { receiver, access } => {
                format!("(?. {} {})", dump(receiver), dump(access))
            }
            ExprKind::ConditionalReceiver => String::from("<recv>"),
            ExprKind::This => String::from("this"),
            ExprKind::Base => String::from("base"),
            ExprKind::Parenthesized(inner) => format!("(paren {})", dump(inner)),
            ExprKind::InterpolatedString(parts) => {
                let mut out = String::from("(interp");
                for part in parts {
                    match part {
                        InterpolationPart::Literal(units) => {
                            out.push_str(&format!(" [{}]", units.len()));
                        }
                        InterpolationPart::Hole {
                            expression,
                            alignment,
                            format,
                        } => {
                            out.push_str(&format!(" {{{}", dump(expression)));
                            if let Some(alignment) = alignment {
                                out.push_str(&format!(",{}", dump(alignment)));
                            }
                            if let Some(format) = format {
                                out.push_str(&format!(":{format}"));
                            }
                            out.push('}');
                        }
                    }
                }
                out.push(')');
                out
            }
            ExprKind::MemberAccess { receiver, name } => {
                format!("(. {} {name})", dump(receiver))
            }
            ExprKind::ConstructedType {
                name,
                type_arguments,
            } => {
                format!("(ctype {} {})", dump(name), type_arguments.len())
            }
            ExprKind::Invocation {
                receiver,
                type_arguments,
                arguments,
            } if type_arguments.is_empty() => {
                format!("(call {}{})", dump(receiver), dump_args(arguments))
            }
            ExprKind::Invocation {
                receiver,
                type_arguments,
                arguments,
            } => format!(
                "(call<{}> {}{})",
                type_arguments
                    .iter()
                    .map(dump_type)
                    .collect::<Vec<_>>()
                    .join(","),
                dump(receiver),
                dump_args(arguments)
            ),
            ExprKind::ElementAccess {
                receiver,
                arguments,
            } => format!("(index {}{})", dump(receiver), dump_args(arguments)),
            ExprKind::RefArgument { out, operand, .. } => {
                format!("({} {})", if *out { "out" } else { "ref" }, dump(operand))
            }
            ExprKind::Unary { operator, operand } => {
                format!("({} {})", unary_text(*operator), dump(operand))
            }
            ExprKind::Await(operand) => format!("(await {})", dump(operand)),
            ExprKind::PostfixUnary { operator, operand } => {
                let text = match operator {
                    PostfixOperator::Increment => "post++",
                    PostfixOperator::Decrement => "post--",
                };
                format!("({text} {})", dump(operand))
            }
            ExprKind::Binary {
                operator,
                left,
                right,
            } => format!(
                "({} {} {})",
                binary_text(*operator),
                dump(left),
                dump(right)
            ),
            ExprKind::Throw(operand) => format!("(throw {})", dump(operand)),
            ExprKind::NullCoalescing { left, right } => {
                format!("(?? {} {})", dump(left), dump(right))
            }
            ExprKind::Conditional {
                condition,
                when_true,
                when_false,
            } => format!(
                "(?: {} {} {})",
                dump(condition),
                dump(when_true),
                dump(when_false)
            ),
            ExprKind::Assignment {
                operator,
                target,
                value,
            } => format!(
                "({} {} {})",
                assignment_text(*operator),
                dump(target),
                dump(value)
            ),
            ExprKind::TypeOf(target) => format!("(typeof {})", dump_type(target)),
            ExprKind::SizeOf(target) => format!("(sizeof {})", dump_type(target)),
            ExprKind::DefaultValue(target) => format!("(default {})", dump_type(target)),
            ExprKind::StackAlloc { element, count } => {
                format!("(stackalloc {} {})", dump_type(element), dump(count))
            }
            ExprKind::Dereference(operand) => format!("(deref {})", dump(operand)),
            ExprKind::AddressOf(operand) => format!("(addressof {})", dump(operand)),
            ExprKind::Checked(inner) => format!("(checked {})", dump(inner)),
            ExprKind::Unchecked(inner) => format!("(unchecked {})", dump(inner)),
            ExprKind::MakeRef(operand) => format!("(makeref {})", dump(operand)),
            ExprKind::RefType(operand) => format!("(reftype {})", dump(operand)),
            ExprKind::RefValue { reference, target } => {
                format!("(refvalue {} {})", dump(reference), dump_type(target))
            }
            ExprKind::ArgListHandle => String::from("(arglist-handle)"),
            ExprKind::ArgListCall(arguments) => {
                let mut text = String::from("(arglist");
                for argument in arguments {
                    text.push(' ');
                    text.push_str(&dump_arg(argument));
                }
                text.push(')');
                text
            }
            ExprKind::TypeTest {
                operation,
                operand,
                target,
            } => {
                let text = match operation {
                    TypeTestOperation::Is => "is",
                    TypeTestOperation::As => "as",
                };
                let right = match target {
                    Pattern::Type(ty) => dump_type(ty),
                    Pattern::Constant(value) => dump(value),
                    Pattern::Discard(_) => String::from("_"),

                    Pattern::Declaration { ty, name, .. } => {
                        format!("{} {name}", dump_type(ty))
                    }
                };
                format!("({text} {} {right})", dump(operand))
            }
            ExprKind::SwitchExpression { governing, arms, .. } => {
                let mut out = format!("(switchexpr {}", dump(governing));
                for arm in arms {
                    let pattern = match &arm.pattern {
                        Pattern::Type(ty) => dump_type(ty),
                        Pattern::Constant(value) => dump(value),
                        Pattern::Discard(_) => String::from("_"),
                        Pattern::Declaration { ty, name, .. } => {
                            format!("{} {name}", dump_type(ty))
                        }
                    };
                    match &arm.guard {
                        Some(guard) => {
                            out.push_str(&format!(" (arm {pattern} when {} {})", dump(guard), dump(&arm.value)));
                        }
                        None => out.push_str(&format!(" (arm {pattern} {})", dump(&arm.value))),
                    }
                }
                out.push(')');
                out
            }
            ExprKind::Cast { target, operand } => {
                format!("(cast {} {})", dump_type(target), dump(operand))
            }
            ExprKind::DeclarationExpression { ty, name } => {
                format!("(decl {} {name})", dump_type(ty))
            }
            ExprKind::ObjectCreation {
                target,
                arguments,
                initializer,
            } => {
                let named = match target {
                    Some(target) => alloc::format!(" {}", dump_type(target)),
                    None => String::new(),
                };
                let mut text = format!("(new{named}{}", dump_args(arguments));
                if let Some(initializer) = initializer {
                    text.push(' ');
                    text.push_str(&dump_initializer(initializer));
                }
                text.push(')');
                text
            }
            ExprKind::ArrayCreation {
                element,
                lengths,
                rank,
                extra_ranks,
                initializer,
            } => {
                let mut text = format!("(newarr {} r{rank}", dump_type(element));
                for length in lengths {
                    text.push(' ');
                    text.push_str(&dump(length));
                }
                for extra in extra_ranks {
                    text.push_str(&format!(" +r{extra}"));
                }
                if let Some(initializer) = initializer {
                    text.push(' ');
                    text.push_str(&dump(initializer));
                }
                text.push(')');
                text
            }
            ExprKind::ArrayInitializer(elements) => {
                let mut text = String::from("{");
                for (index, element) in elements.iter().enumerate() {
                    if index > 0 {
                        text.push(' ');
                    }
                    text.push_str(&dump(element));
                }
                text.push('}');
                text
            }
            ExprKind::Deconstruction {
                var_span,
                targets,
                value,
            } => {
                let mut text = String::from(if var_span.is_some() {
                    "(deconstruct-var"
                } else {
                    "(deconstruct"
                });
                for target in targets {
                    text.push(' ');
                    text.push_str(&dump_target(target));
                }
                text.push_str(&format!(" = {})", dump(value)));
                text
            }
            ExprKind::Error => String::from("<error>"),
        }
    }

    /// One deconstruction target, in the same prefix form as [`dump`].
    fn dump_target(target: &DeconstructionTarget) -> String {
        match target {
            DeconstructionTarget::Declaration { ty, name, .. } => match ty {
                Some(ty) => format!("(decl {} {name})", dump_type(ty)),
                None => format!("(decl var {name})"),
            },
            DeconstructionTarget::Discard(_) => String::from("_"),
            DeconstructionTarget::Expression(expr) => dump(expr),
            DeconstructionTarget::Nested { targets, .. } => {
                let mut text = String::from("(nested");
                for target in targets {
                    text.push(' ');
                    text.push_str(&dump_target(target));
                }
                text.push(')');
                text
            }
        }
    }

    fn dump_exprs(expressions: &[Expr]) -> String {
        let mut text = String::new();
        for expression in expressions {
            text.push(' ');
            text.push_str(&dump(expression));
        }
        text
    }

    /// One argument, `name:` first when it has one. **THE ONE PLACE THAT RENDERS AN ARGUMENT** --
    /// four call sites reached for it the moment arguments could be named, and a fifth is a matter
    /// of time.
    fn dump_arg(argument: &Argument) -> String {
        match &argument.name {
            Some(name) => format!("{}:{}", name.text, dump(&argument.value)),
            None => dump(&argument.value),
        }
    }

    fn dump_args(arguments: &[Argument]) -> String {
        let mut text = String::new();
        for argument in arguments {
            text.push(' ');
            text.push_str(&dump_arg(argument));
        }
        text
    }

    /// Renders a type reference, element type first, which matches C# surface
    /// order for the single-rank and jagged cases the tests use.
    fn dump_type(ty: &TypeRef) -> String {
        match &ty.kind {
            TypeRefKind::Tuple(elements) => {
                let mut text = String::from("(tuple");
                for element in elements {
                    text.push(' ');
                    if let Some(name) = &element.name {
                        text.push_str(&name.text);
                        text.push(':');
                    }
                    text.push_str(&dump_type(&element.ty));
                }
                text.push(')');
                text
            }
            TypeRefKind::Predefined(predefined) => String::from(predefined_text(*predefined)),
            TypeRefKind::Name(parts) => {
                let mut text = String::new();
                for (index, part) in parts.iter().enumerate() {
                    if index > 0 {
                        text.push('.');
                    }
                    text.push_str(part);
                }
                text
            }
            TypeRefKind::Nullable(underlying) => format!("{}?", dump_type(underlying)),
            TypeRefKind::Generic { parts } => {
                let mut text = String::new();
                for (index, part) in parts.iter().enumerate() {
                    if index > 0 {
                        text.push('.');
                    }
                    text.push_str(&part.name);
                    if part.arguments.is_empty() {
                        continue;
                    }
                    text.push('<');
                    for (index, argument) in part.arguments.iter().enumerate() {
                        if index > 0 {
                            text.push(',');
                        }
                        text.push_str(&dump_type(argument));
                    }
                    text.push('>');
                }
                text
            }
            TypeRefKind::Unbound { parts, arity } => {
                let mut text = parts.join(".");
                text.push('<');
                for _ in 1..*arity {
                    text.push(',');
                }
                text.push('>');
                text
            }
            TypeRefKind::Array { element, rank } => {
                let mut text = dump_type(element);
                text.push('[');
                for _ in 1..*rank {
                    text.push(',');
                }
                text.push(']');
                text
            }
            TypeRefKind::Pointer(element) => format!("{}*", dump_type(element)),
            TypeRefKind::ByRef {
                referent,
                is_readonly,
            } => format!(
                "{}{}",
                if *is_readonly { "ref readonly " } else { "ref " },
                dump_type(referent)
            ),
            TypeRefKind::Error => String::from("<error-type>"),
        }
    }

    fn predefined_text(predefined: PredefinedType) -> &'static str {
        match predefined {
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

    fn unary_text(operator: UnaryOperator) -> &'static str {
        match operator {
            UnaryOperator::Plus => "+",
            UnaryOperator::Minus => "-",
            UnaryOperator::Not => "!",
            UnaryOperator::Complement => "~",
            UnaryOperator::PreIncrement => "pre++",
            UnaryOperator::PreDecrement => "pre--",
        }
    }

    fn binary_text(operator: BinaryOperator) -> &'static str {
        match operator {
            BinaryOperator::Multiply => "*",
            BinaryOperator::Divide => "/",
            BinaryOperator::Modulo => "%",
            BinaryOperator::Add => "+",
            BinaryOperator::Subtract => "-",
            BinaryOperator::LeftShift => "<<",
            BinaryOperator::RightShift => ">>",
            BinaryOperator::LessThan => "<",
            BinaryOperator::GreaterThan => ">",
            BinaryOperator::LessThanOrEqual => "<=",
            BinaryOperator::GreaterThanOrEqual => ">=",
            BinaryOperator::Equal => "==",
            BinaryOperator::NotEqual => "!=",
            BinaryOperator::BitwiseAnd => "&",
            BinaryOperator::BitwiseXor => "^",
            BinaryOperator::BitwiseOr => "|",
            BinaryOperator::LogicalAnd => "&&",
            BinaryOperator::LogicalOr => "||",
        }
    }

    fn assignment_text(operator: AssignmentOperator) -> &'static str {
        match operator {
            AssignmentOperator::Assign => "=",
            AssignmentOperator::Add => "+=",
            AssignmentOperator::Subtract => "-=",
            AssignmentOperator::Multiply => "*=",
            AssignmentOperator::Divide => "/=",
            AssignmentOperator::Modulo => "%=",
            AssignmentOperator::And => "&=",
            AssignmentOperator::Or => "|=",
            AssignmentOperator::Xor => "^=",
            AssignmentOperator::LeftShift => "<<=",
            AssignmentOperator::RightShift => ">>=",
        }
    }

    /// Parses `source` under `version` with no diagnostics expected, returning the dumped tree.
    ///
    /// Needed for constructs the DEFAULT dialect refuses: `tree` asserts a clean parse, and a
    /// C# 3 initializer under C# 1.0 draws the gate, so asserting its SHAPE means asking for it in
    /// a dialect that admits it.
    fn tree_at(source: &str, version: LanguageVersion) -> String {
        let mut parser = Parser::new(tokenize_with(
            source,
            LexOptions {
                version,
                ..LexOptions::default()
            },
        ));
        parser.version = version;
        let expr = parser.parse_expression();
        let diagnostics = without_gated_operator_cascades(parser.diagnostics);
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics for {source:?}: {diagnostics:?}"
        );
        dump(&expr)
    }

    /// Parses `source` with no diagnostics expected, returning the dumped tree.
    fn tree(source: &str) -> String {
        let parsed = parse_expression(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "unexpected diagnostics for {source:?}: {:?}",
            parsed.diagnostics
        );
        dump(&parsed.expr)
    }

    fn codes(source: &str) -> Vec<u16> {
        parse_expression(source)
            .diagnostics
            .iter()
            .map(Diagnostic::code)
            .collect()
    }

    /// The diagnostic codes an expression draws at a NAMED rung.
    ///
    fn codes_at(source: &str, version: LanguageVersion) -> Vec<u16> {
        let mut parser = Parser::new(tokenize_with(
            source,
            LexOptions {
                version,
                ..LexOptions::default()
            },
        ));
        parser.version = version;
        let expr = parser.parse_expression();
        drop(expr);
        without_gated_operator_cascades(parser.diagnostics)
            .iter()
            .map(Diagnostic::code)
            .collect()
    }

    /// Renders a statement as a parenthesized prefix form, like [`dump`].
    fn dump_stmt(statement: &Stmt) -> String {
        match &statement.kind {
            StmtKind::Block(statements) => {
                let mut text = String::from("(block");
                for inner in statements {
                    text.push(' ');
                    text.push_str(&dump_stmt(inner));
                }
                text.push(')');
                text
            }
            StmtKind::Empty => String::from("(empty)"),
            StmtKind::Expression(expr) => format!("(expr {})", dump(expr)),
            StmtKind::LocalDeclaration {
                ty,
                declarators,
                is_const,
            } => {
                let mut text =
                    format!("(local{} {}", if *is_const { " const" } else { "" }, dump_type(ty));
                for declarator in declarators {
                    match &declarator.initializer {
                        Some(initializer) => {
                            text.push_str(&format!(" {}={}", declarator.name, dump(initializer)));
                        }
                        None => text.push_str(&format!(" {}", declarator.name)),
                    }
                }
                text.push(')');
                text
            }
            StmtKind::Return(None) => String::from("(return)"),
            StmtKind::Return(Some(expr)) => format!("(return {})", dump(expr)),
            StmtKind::If {
                condition,
                then_branch,
                else_branch,
            } => match else_branch {
                Some(otherwise) => format!(
                    "(if {} {} {})",
                    dump(condition),
                    dump_stmt(then_branch),
                    dump_stmt(otherwise)
                ),
                None => format!("(if {} {})", dump(condition), dump_stmt(then_branch)),
            },
            StmtKind::While { condition, body } => {
                format!("(while {} {})", dump(condition), dump_stmt(body))
            }
            StmtKind::DoWhile { body, condition } => {
                format!("(do {} {})", dump_stmt(body), dump(condition))
            }
            StmtKind::For {
                initializer,
                condition,
                iterators,
                body,
            } => {
                let init = match initializer {
                    None => String::from("_"),
                    Some(ForInitializer::Declaration { ty, declarators }) => {
                        let mut text = format!("(local {}", dump_type(ty));
                        for declarator in declarators {
                            match &declarator.initializer {
                                Some(value) => {
                                    text.push_str(&format!(" {}={}", declarator.name, dump(value)));
                                }
                                None => text.push_str(&format!(" {}", declarator.name)),
                            }
                        }
                        text.push(')');
                        text
                    }
                    Some(ForInitializer::Expressions(expressions)) => {
                        format!("(exprs{})", dump_exprs(expressions))
                    }
                };
                let cond = match condition {
                    Some(condition) => dump(condition),
                    None => String::from("_"),
                };
                let iters = if iterators.is_empty() {
                    String::from("_")
                } else {
                    format!("(iters{})", dump_exprs(iterators))
                };
                format!("(for {init} {cond} {iters} {})", dump_stmt(body))
            }
            StmtKind::ForEach {
                ty,
                name,
                collection,
                body,
            } => format!(
                "(foreach {} {name} {} {})",
                dump_type(ty),
                dump(collection),
                dump_stmt(body)
            ),
            StmtKind::ForEachDeconstruction {
                var_span,
                targets,
                collection,
                body,
            } => {
                let mut text = String::from(if var_span.is_some() {
                    "(foreach-deconstruct-var"
                } else {
                    "(foreach-deconstruct"
                });
                for target in targets {
                    text.push(' ');
                    text.push_str(&dump_target(target));
                }
                text.push_str(&format!(
                    " in {} {})",
                    dump(collection),
                    dump_stmt(body)
                ));
                text
            }
            StmtKind::Break => String::from("(break)"),
            StmtKind::Continue => String::from("(continue)"),
            StmtKind::Throw(None) => String::from("(throw)"),
            StmtKind::Throw(Some(expr)) => format!("(throw {})", dump(expr)),
            StmtKind::Try {
                body,
                catches,
                finally_block,
            } => {
                let mut text = format!("(try {}", dump_stmt(body));
                for clause in catches {
                    text.push_str(" (catch");
                    if let Some(ty) = &clause.exception_type {
                        text.push_str(&format!(" {}", dump_type(ty)));
                    }
                    if let Some(name) = &clause.name {
                        text.push_str(&format!(" {name}"));
                    }
                    text.push_str(&format!(" {})", dump_stmt(&clause.body)));
                }
                if let Some(finally_block) = finally_block {
                    text.push_str(&format!(" (finally {})", dump_stmt(finally_block)));
                }
                text.push(')');
                text
            }
            StmtKind::Lock { expression, body } => {
                format!("(lock {} {})", dump(expression), dump_stmt(body))
            }
            StmtKind::Fixed {
                ty,
                name,
                init,
                body,
            } => format!(
                "(fixed {} {} {} {})",
                dump_type(ty),
                name,
                dump(init),
                dump_stmt(body)
            ),
            StmtKind::Using { resource, body } => {
                let res = match resource {
                    UsingResource::Declaration { ty, declarators } => {
                        let mut text = format!("(local {}", dump_type(ty));
                        for declarator in declarators {
                            match &declarator.initializer {
                                Some(value) => {
                                    text.push_str(&format!(" {}={}", declarator.name, dump(value)));
                                }
                                None => text.push_str(&format!(" {}", declarator.name)),
                            }
                        }
                        text.push(')');
                        text
                    }
                    UsingResource::Expression(expr) => dump(expr),
                };
                format!("(using {res} {})", dump_stmt(body))
            }
            StmtKind::Checked(body) => format!("(checked-block {})", dump_stmt(body)),
            StmtKind::Unchecked(body) => format!("(unchecked-block {})", dump_stmt(body)),
            StmtKind::Switch {
                expression,
                sections,
            } => {
                let mut text = format!("(switch {}", dump(expression));
                for section in sections {
                    text.push_str(" (section");
                    for label in &section.labels {
                        match label {
                            SwitchLabel::Case(value) => {
                                text.push_str(&format!(" (case {})", dump(value)));
                            }
                            SwitchLabel::Default => text.push_str(" (default)"),
                        }
                    }
                    for statement in &section.statements {
                        text.push(' ');
                        text.push_str(&dump_stmt(statement));
                    }
                    text.push(')');
                }
                text.push(')');
                text
            }
            StmtKind::Labeled { label, statement } => {
                format!("(label {label} {})", dump_stmt(statement))
            }
            StmtKind::Goto(target) => match target {
                GotoTarget::Label(name) => format!("(goto {name})"),
                GotoTarget::Case(value) => format!("(goto-case {})", dump(value)),
                GotoTarget::Default => String::from("(goto-default)"),
            },
            StmtKind::Error => String::from("<error-stmt>"),
        }
    }

    fn stmt_tree(source: &str) -> String {
        let parsed = parse_statement(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "unexpected diagnostics for {source:?}: {:?}",
            parsed.diagnostics
        );
        dump_stmt(&parsed.statement)
    }

    fn stmt_codes(source: &str) -> Vec<u16> {
        parse_statement(source)
            .diagnostics
            .iter()
            .map(Diagnostic::code)
            .collect()
    }

    #[test]
    fn literals_and_names() {
        assert_eq!(tree("42"), "42");
        assert_eq!(tree("true"), "true");
        assert_eq!(tree("null"), "null");
        assert_eq!(tree("foo"), "foo");
        assert_eq!(tree("this"), "this");
    }

    /// The null-forgiving operator is consumed and DISCARDED, so what it leaves is the operand's
    /// own tree.
    ///
    /// It suppresses a nullable-analysis warning and does nothing else -- no type change, no
    /// evaluation, no IL -- and this build has no nullable analysis for it to speak to. A node
    /// would carry nothing and every later pass would have to know to look through it.
    #[test]
    fn a_null_forgiving_operator_leaves_the_expression_it_suppressed() {
        assert_eq!(tree("s!"), tree("s"));
        assert_eq!(tree("s!.Length"), tree("s.Length"));
        assert_eq!(tree("a[0]!.Length"), tree("a[0].Length"));
        assert_eq!(tree("s!.Trim()!.Length"), tree("s.Trim().Length"));
        assert_eq!(tree("((string)o)!.Length"), tree("((string)o).Length"));
    }

    /// An inequality is ONE token, so it can never reach the suppression arm -- and a suppression
    /// followed by a comparison is the case that would break if it could.
    ///
    /// **THE ASSERTIONS NAME THE SHAPE RATHER THAN THE SILENCE**, and the first version of this
    /// test did not. It compared `s! != t` against `s != t`, and a perturbation that let `!=`
    /// reach the suppression arm collapsed BOTH sides to `s` -- equal, and both wrong. Two
    /// expressions that degrade the same way cannot check each other.
    #[test]
    fn an_inequality_is_not_a_null_suppression() {
        assert_eq!(tree("a != b"), "(!= a b)");
        assert_eq!(tree("s! != t"), "(!= s t)");
        assert_eq!(tree("!b"), "(! b)");
        assert_eq!(tree("!a == b"), "(== (! a) b)");
    }

    /// Asserting twice says nothing the first assertion did not, and csc refuses it rather than
    /// folding it away. A build that treats the operator as transparent still has to count them.
    #[test]
    fn two_null_suppressions_in_a_row_are_refused() {
        assert!(codes("s!").is_empty());
        assert_eq!(codes("s!!"), vec![8715]);
        assert_eq!(codes("s!!!"), vec![8715, 8715]);
    }

    /// C# 8.0.
    ///
    /// The diagnostic carries the code for the language version IN EFFECT, not for the one the
    /// feature requires: a 7.3 compilation reports `CS8370`, which is 7.3's code, rather than an
    /// 8.0 one.
    #[test]
    fn a_null_forgiving_operator_needs_csharp_8() {
        assert_eq!(codes_at("s!", LanguageVersion::CSharp7_3), vec![8370]);
        assert!(codes_at("s!", LanguageVersion::CSharp8).is_empty());
    }

    /// **THE EXPRESSION HALF OF 9.4.2, WHICH NO TOKEN-LEVEL GUARD CAN COVER.**
    /// [`Parser::current_contextual_keyword`] serves the sites that still hold a token; a BINDER
    /// site asking whether a simple name is a contextual keyword -- `nameof` today, `with`
    /// tomorrow -- sees an [`Expr`] whose text has already had the `@` dropped, so the flag has to
    /// reach the tree. Until it did, `@nameof(x)` compiled here and is `CS0103` under csc,
    /// measured.
    ///
    /// The name itself stays `nameof`, because that is the identifier `@nameof` denotes and what
    /// it must bind to; only the dump renders the `@`, so the flag is visible to a test at all.
    #[test]
    fn a_verbatim_simple_name_records_its_at_sign() {
        assert_eq!(tree("nameof(x)"), "(call nameof x)");
        assert_eq!(tree("@nameof(x)"), "(call @nameof x)");
        assert_eq!(tree("@x + y"), "(+ @x y)");
        assert_eq!(tree("x + @y"), "(+ x @y)");
    }

    /// A THROW EXPRESSION'S OPERAND STOPS BELOW `??` AND `?:`, and the whole feature is where the
    /// resulting tree puts it -- so these are asserted as TREES. An operand parsed as a full
    /// expression swallows both operators, and the refusal that follows is then about the wrong
    /// expression: `throw e ?? s` would be one throw of a coalesce rather than a coalesce whose
    /// left operand is a throw, and csc's CS0019 for that source names `Exception` and `string`,
    /// which only the second grouping can produce.
    #[test]
    fn a_throw_expression_takes_an_operand_below_coalescing_and_the_conditional() {
        let v7 = LanguageVersion::CSharp7;
        assert_eq!(tree_at("a ?? throw e", v7), "(?? a (throw e))");
        assert_eq!(tree_at("throw e ?? s", v7), "(?? (throw e) s)");
        assert_eq!(tree_at("throw e ? a : b", v7), "(?: (throw e) a b)");
        assert_eq!(tree_at("c ? throw e : b", v7), "(?: c (throw e) b)");
        assert_eq!(tree_at("c ? a : throw e", v7), "(?: c a (throw e))");
        assert_eq!(tree_at("a ?? throw f(x)", v7), "(?? a (throw (call f x)))");
        assert_eq!(codes_at("1 + throw e", v7), [1525]);
        assert_eq!(codes_at("a ?? throw e", LanguageVersion::CSharp5), [8026]);
        assert_eq!(codes_at("a ?? throw e", LanguageVersion::CSharp6), [8059]);
    }

    /// `??` sits between conditional-OR and `?:` and is the one RIGHT-associative binary spelling
    /// in the language (14.13). Both facts are asserted as TREES rather than as "it compiles",
    /// because a left-associative `??` accepts every program a right-associative one does and
    /// answers differently only when the middle operand is null.
    #[test]
    fn null_coalescing_is_right_associative_and_binds_below_the_conditional() {
        let v2 = LanguageVersion::CSharp2;
        assert_eq!(tree_at("a ?? b", v2), "(?? a b)");
        assert_eq!(tree_at("a ?? b ?? c", v2), "(?? a (?? b c))");
        assert_eq!(tree_at("a ?? b ? c : d", v2), "(?: (?? a b) c d)");
        assert_eq!(tree_at("a || b ?? c", v2), "(?? (|| a b) c)");
        assert_eq!(tree_at("x = a ?? b", v2), "(= x (?? a b))");
        assert_eq!(codes_at("a ?? b", LanguageVersion::CSharp1), [8022]);
    }

    #[test]
    fn multiplication_binds_tighter_than_addition() {
        assert_eq!(tree("1 + 2 * 3"), "(+ 1 (* 2 3))");
        assert_eq!(tree("1 * 2 + 3"), "(+ (* 1 2) 3)");
    }

    #[test]
    fn binary_operators_are_left_associative() {
        assert_eq!(tree("1 - 2 - 3"), "(- (- 1 2) 3)");
        assert_eq!(tree("a / b / c"), "(/ (/ a b) c)");
    }

    #[test]
    fn typedref_operators_parse_only_under_the_knob() {
        let typedref = LexOptions {
            typedref: true,
            ..LexOptions::default()
        };
        let parse = |source: &str| -> String {
            let mut parser = Parser::new(tokenize_with(source, typedref.clone()));
            let expr = parser.parse_expression();
            assert!(
                parser.diagnostics.is_empty(),
                "unexpected diagnostics for {source:?}: {:?}",
                parser.diagnostics
            );
            dump(&expr)
        };
        assert_eq!(parse("__makeref(x)"), "(makeref x)");
        assert_eq!(parse("__reftype(tr)"), "(reftype tr)");
        assert_eq!(parse("__refvalue(tr, int)"), "(refvalue tr int)");
        assert_eq!(tree("__makeref(x)"), "(call __makeref x)");
    }

    #[test]
    fn arglist_expressions_parse_only_under_the_knob() {
        let typedref = LexOptions {
            typedref: true,
            ..LexOptions::default()
        };
        let parse = |source: &str| -> String {
            let mut parser = Parser::new(tokenize_with(source, typedref.clone()));
            let expr = parser.parse_expression();
            assert!(
                parser.diagnostics.is_empty(),
                "unexpected diagnostics for {source:?}: {:?}",
                parser.diagnostics
            );
            dump(&expr)
        };
        assert_eq!(parse("__arglist(2, x)"), "(arglist 2 x)");
        assert_eq!(parse("__arglist()"), "(arglist)");
        assert_eq!(parse("__arglist"), "(arglist-handle)");
        assert_eq!(
            parse("new ArgIterator(__arglist)"),
            "(new ArgIterator (arglist-handle))"
        );
        assert_eq!(tree("__arglist(x)"), "(call __arglist x)");
    }

    #[test]
    fn arglist_parameter_marks_a_vararg_method_or_constructor() {
        let typedref = LexOptions {
            typedref: true,
            ..LexOptions::default()
        };
        let parse_unit = |source: &str| -> String {
            let parsed = parse_compilation_unit_with(source, typedref.clone());
            assert!(
                parsed.diagnostics.is_empty(),
                "unexpected diagnostics for {source:?}: {:?}",
                parsed.diagnostics
            );
            dump_unit(&parsed.unit)
        };
        assert_eq!(
            parse_unit("class C { static int Sum(int seed, __arglist) { return seed; } }"),
            "(class C (method static int Sum (int seed, __arglist) (block (return seed))))"
        );
        assert_eq!(
            parse_unit("class T { public T(__arglist) { } }"),
            "(class T (ctor public T (__arglist) (block)))"
        );
        assert_eq!(
            parse_unit("interface I { void M(__arglist); }"),
            "(interface I (method void M (__arglist) ;))"
        );
    }

    #[test]
    fn arglist_not_last_is_cs0257_and_still_marks_the_member() {
        let typedref = LexOptions {
            typedref: true,
            ..LexOptions::default()
        };
        let parsed = parse_compilation_unit_with(
            "class C { static void M(__arglist, int a) { } }",
            typedref.clone(),
        );
        let codes: Vec<u16> = parsed.diagnostics.iter().map(Diagnostic::code).collect();
        assert_eq!(codes, vec![257]);
        assert_eq!(
            dump_unit(&parsed.unit),
            "(class C (method static void M (int a, __arglist) (block)))"
        );
    }

    #[test]
    fn arglist_in_a_non_vararg_context_is_cs1669() {
        let typedref = LexOptions {
            typedref: true,
            ..LexOptions::default()
        };
        let codes = |source: &str| -> Vec<u16> {
            parse_compilation_unit_with(source, typedref.clone())
                .diagnostics
                .iter()
                .map(Diagnostic::code)
                .collect()
        };
        assert_eq!(codes("delegate void D(__arglist);"), vec![1669]);
        assert_eq!(
            codes("class C { public static int operator +(C a, __arglist) { return 1; } }"),
            vec![1669]
        );
        assert_eq!(
            codes("class C { int this[__arglist] { get { return 1; } } }"),
            vec![1669]
        );
    }

    #[test]
    fn the_precedence_ladder_matches_the_grammar() {
        assert_eq!(tree("a || b && c"), "(|| a (&& b c))");
        assert_eq!(tree("a == b && c"), "(&& (== a b) c)");
        assert_eq!(tree("a | b ^ c & d"), "(| a (^ b (& c d)))");
        assert_eq!(tree("a == b | c"), "(| (== a b) c)");
        assert_eq!(tree("a < b << c"), "(< a (<< b c))");
    }

    #[test]
    fn parentheses_group() {
        assert_eq!(tree("(1 + 2) * 3"), "(* (paren (+ 1 2)) 3)");
    }

    #[test]
    fn unary_binds_tighter_than_binary_and_nests() {
        assert_eq!(tree("-a * b"), "(* (- a) b)");
        assert_eq!(tree("!a == b"), "(== (! a) b)");
        assert_eq!(tree("- - a"), "(- (- a))");
    }

    #[test]
    fn postfix_binds_tighter_than_prefix() {
        assert_eq!(tree("a++"), "(post++ a)");
        assert_eq!(tree("++a"), "(pre++ a)");
        assert_eq!(tree("-a++"), "(- (post++ a))");
    }

    #[test]
    fn member_access_invocation_and_indexing() {
        assert_eq!(tree("a.b.c"), "(. (. a b) c)");
        assert_eq!(tree("f()"), "(call f)");
        assert_eq!(tree("f(x, y)"), "(call f x y)");
        assert_eq!(tree("a[i]"), "(index a i)");
        assert_eq!(tree("a.b(c)[d]"), "(index (call (. a b) c) d)");
    }

    #[test]
    fn an_argument_keeps_the_name_it_was_written_with() {
        assert_eq!(tree("f(x: 1)"), "(call f x:1)");
        assert_eq!(tree("f(1, y: 2)"), "(call f 1 y:2)");
        assert_eq!(tree("c[b: 1, a: 43]"), "(index c b:1 a:43)");
        assert_eq!(tree("new C(b: 1, a: 43)"), "(new C b:1 a:43)");
        assert_eq!(tree("f(b: ref x)"), "(call f b:(ref x))");
        assert_eq!(tree("f(b: out x)"), "(call f b:(out x))");
        assert_eq!(tree("f(a ? b : c)"), "(call f (?: a b c))");
        assert_eq!(tree("f(n: a ? b : c)"), "(call f n:(?: a b c))");
    }

    #[test]
    fn an_element_access_needs_at_least_one_index() {
        assert!(codes("a[]").contains(&443));
        assert!(codes("new C()[]").contains(&443));
        assert_eq!(tree("new int[]{1}"), "(newarr int r1 {1})");
        assert_eq!(tree("new int[n][]"), "(newarr int r1 n +r1)");
        assert!(codes("typeof(int[])").is_empty());
        assert!(codes("a[0]").is_empty());
    }

    #[test]
    fn an_array_size_list_has_no_named_form() {
        assert_eq!(tree("new int[n]"), "(newarr int r1 n)");
        assert!(codes("new int[n: 3]").contains(&1003));
    }

    #[test]
    fn the_conditional_is_lower_than_binary_and_chains_on_the_right() {
        assert_eq!(tree("a ? b : c"), "(?: a b c)");
        assert_eq!(tree("a || b ? c : d"), "(?: (|| a b) c d)");
        assert_eq!(tree("a ? b : c ? d : e"), "(?: a b (?: c d e))");
    }

    #[test]
    fn assignment_is_lowest_and_right_associative() {
        assert_eq!(tree("a = b = c"), "(= a (= b c))");
        assert_eq!(tree("a = b ? c : d"), "(= a (?: b c d))");
        assert_eq!(tree("x += 1"), "(+= x 1)");
        assert_eq!(tree("total >>= shift"), "(>>= total shift)");
    }

    #[test]
    fn a_missing_operand_is_cs1525() {
        assert_eq!(codes("1 +"), vec![1525]);
        assert_eq!(codes(""), vec![1525]);
    }

    #[test]
    fn a_missing_closer_is_reported() {
        assert!(codes("(1 + 2").contains(&1026));
        assert!(codes("a[i").contains(&1003));
        assert!(codes("a ? b").contains(&1003));
    }

    #[test]
    fn a_missing_comma_in_an_argument_list_is_one_cs1003() {
        assert_eq!(codes("f(a b)"), vec![1003]);
        assert_eq!(codes("f(a b c)"), vec![1003, 1003]);
        assert_eq!(codes("a[i j]"), vec![1003]);
        assert_eq!(codes("f(a"), vec![1026]);
    }

    #[test]
    fn a_member_access_without_a_name_is_cs1001() {
        assert_eq!(codes("a."), vec![1001]);
    }

    #[test]
    fn typeof_takes_a_type_including_arrays() {
        assert_eq!(tree("typeof(int)"), "(typeof int)");
        assert_eq!(tree("typeof(string)"), "(typeof string)");
        assert_eq!(tree("typeof(A.B.C)"), "(typeof A.B.C)");
        assert_eq!(tree("typeof(int[])"), "(typeof int[])");
        assert_eq!(tree("typeof(int[,])"), "(typeof int[,])");
        assert_eq!(tree("typeof(int[][])"), "(typeof int[][])");
    }

    #[test]
    fn base_access() {
        assert_eq!(tree("base.x"), "(. base x)");
        assert_eq!(tree("base.M(a)"), "(call (. base M) a)");
        assert_eq!(tree("base[i]"), "(index base i)");
    }

    #[test]
    fn checked_and_unchecked_wrap_an_expression() {
        assert_eq!(tree("checked(a + b)"), "(checked (+ a b))");
        assert_eq!(tree("unchecked(x)"), "(unchecked x)");
    }

    #[test]
    fn is_and_as_take_a_type_at_relational_precedence() {
        assert_eq!(tree("x is string"), "(is x string)");
        assert_eq!(tree("x as object"), "(as x object)");
        assert_eq!(tree("o is A.B"), "(is o A.B)");
        assert_eq!(tree("o is int[]"), "(is o int[])");
        assert_eq!(tree("a + b is int"), "(is (+ a b) int)");
        assert_eq!(tree("x is int == y"), "(== (is x int) y)");
    }

    #[test]
    fn a_constant_pattern_takes_any_constant_and_stops_at_relational() {
        assert_eq!(tree("x is 3"), "(is x 3)");
        assert_eq!(tree("x is -1"), "(is x (- 1))");
        assert_eq!(tree("s is \"a\""), "(is s str)");
        assert_eq!(tree("c is 'z'"), "(is c char:122)");
        assert_eq!(tree("b is true"), "(is b true)");
        assert_eq!(tree("x is null"), "(is x null)");
        assert_eq!(tree("x is 1 + 2"), "(is x (+ 1 2))");
        assert_eq!(tree("x is 3 && y"), "(&& (is x 3) y)");
        assert_eq!(tree("x is 3 == y"), "(== (is x 3) y)");
        assert_eq!(tree("x is 3 ? a : b"), "(?: (is x 3) a b)");
    }

    #[test]
    fn a_switch_expression_is_a_postfix_operator_between_multiplicative_and_unary() {
        assert_eq!(
            tree("x switch { 1 => 10, _ => 0 }"),
            "(switchexpr x (arm 1 10) (arm _ 0))"
        );
        assert_eq!(
            tree("2 * x switch { _ => 0 }"),
            "(* 2 (switchexpr x (arm _ 0)))"
        );
        assert_eq!(
            tree("-x switch { _ => 0 }"),
            "(switchexpr (- x) (arm _ 0))"
        );
        assert_eq!(
            tree("a + x switch { _ => 0 }"),
            "(+ a (switchexpr x (arm _ 0)))"
        );
        assert_eq!(
            tree("x switch { 1 => 2 } switch { 2 => 3 }"),
            "(switchexpr (switchexpr x (arm 1 2)) (arm 2 3))"
        );
        assert_eq!(
            tree("o is string switch { _ => 0 }"),
            "(switchexpr (is o string) (arm _ 0))"
        );
    }

    #[test]
    fn a_switch_arm_reads_a_name_as_a_constant_and_a_when_as_a_guard() {
        assert_eq!(
            tree("e switch { E.A => 1, _ => 0 }"),
            "(switchexpr e (arm (. E A) 1) (arm _ 0))"
        );
        assert_eq!(tree("o is A.B"), "(is o A.B)");
        assert_eq!(
            tree("x switch { _ when b => 1, _ => 0 }"),
            "(switchexpr x (arm _ when b 1) (arm _ 0))"
        );
        assert_eq!(
            tree("o switch { string s when b => 1, _ => 0 }"),
            "(switchexpr o (arm string s when b 1) (arm _ 0))"
        );
        assert_eq!(tree("o is string when"), "(is o string when)");
        assert_eq!(tree("x switch { 1 => 2, }"), "(switchexpr x (arm 1 2))");
        assert_eq!(tree("x switch { }"), "(switchexpr x)");
    }

    #[test]
    fn a_name_after_is_stays_the_c_sharp_1_type_test() {
        assert_eq!(tree("o is A.B"), "(is o A.B)");
        assert_eq!(tree("o is T"), "(is o T)");
        assert_eq!(tree("o is string s"), "(is o string s)");
    }

    #[test]
    fn a_missing_type_is_cs1031() {
        assert_eq!(codes("typeof()"), vec![1031]);
        assert_eq!(codes("x as"), vec![1031]);
        assert_eq!(codes("x is"), vec![8504]);
    }

    #[test]
    fn casts_follow_the_disambiguation_rule() {
        assert_eq!(tree("(int)x"), "(cast int x)");
        assert_eq!(tree("(int[])x"), "(cast int[] x)");
        assert_eq!(tree("(a)b"), "(cast a b)");
        assert_eq!(tree("(a)(b)"), "(cast a (paren b))");
        assert_eq!(tree("(Foo)new Bar()"), "(cast Foo (new Bar))");
        assert_eq!(tree("(a)-b"), "(- (paren a) b)");
        assert_eq!(tree("(a)*b"), "(* (paren a) b)");
        assert_eq!(tree("(int)(long)x"), "(cast int (cast long x))");
    }

    #[test]
    fn a_predefined_type_can_begin_a_static_member_access() {
        assert_eq!(tree("int.Parse(s)"), "(call (. int Parse) s)");
        assert_eq!(tree("string.Empty"), "(. string Empty)");
    }

    #[test]
    fn object_and_array_creation() {
        assert_eq!(tree("new Foo()"), "(new Foo)");
        assert_eq!(tree("new Foo(a, b)"), "(new Foo a b)");
        assert_eq!(tree("new A.B.C(x)"), "(new A.B.C x)");
        assert_eq!(tree("new int[5]"), "(newarr int r1 5)");
        assert_eq!(tree("new int[3, 4]"), "(newarr int r2 3 4)");
        assert_eq!(tree("new int[n][]"), "(newarr int r1 n +r1)");
        assert_eq!(tree("new Foo().Bar"), "(. (new Foo) Bar)");
    }

    #[test]
    fn array_initializers() {
        assert_eq!(tree("new int[] {1, 2, 3}"), "(newarr int r1 {1 2 3})");
        assert_eq!(tree("new int[2] {1, 2}"), "(newarr int r1 2 {1 2})");
        assert_eq!(
            tree("new int[,] {{1, 2}, {3, 4}}"),
            "(newarr int r2 {{1 2} {3 4}})"
        );
        assert_eq!(tree("new int[] {1, 2,}"), "(newarr int r1 {1 2})");
        assert_eq!(
            stmt_tree("int[] a = { 1, 2, 3 };"),
            "(local int[] a={1 2 3})"
        );
        assert_eq!(
            stmt_tree("int[,] m = { {1, 2}, {3, 4} };"),
            "(local int[,] m={{1 2} {3 4}})"
        );
        assert_eq!(
            unit_tree("class C { int[] data = {1, 2}; }"),
            "(class C (field int[] data={1 2}))"
        );
    }

    #[test]
    fn blocks_and_empty_statements() {
        assert_eq!(stmt_tree("{}"), "(block)");
        assert_eq!(stmt_tree("{ ; ; }"), "(block (empty) (empty))");
        assert_eq!(stmt_tree("{ { } }"), "(block (block))");
    }

    #[test]
    fn expression_statements() {
        assert_eq!(stmt_tree("f(x);"), "(expr (call f x))");
        assert_eq!(stmt_tree("a = b;"), "(expr (= a b))");
        assert_eq!(stmt_tree("i++;"), "(expr (post++ i))");
    }

    #[test]
    fn local_variable_declarations() {
        assert_eq!(stmt_tree("int x;"), "(local int x)");
        assert_eq!(stmt_tree("int x = 5;"), "(local int x=5)");
        assert_eq!(stmt_tree("int a = 1, b, c = 3;"), "(local int a=1 b c=3)");
        assert_eq!(stmt_tree("Foo.Bar baz;"), "(local Foo.Bar baz)");
        assert_eq!(stmt_tree("int[] xs;"), "(local int[] xs)");
    }

    #[test]
    fn declaration_versus_expression_is_disambiguated() {
        assert_eq!(stmt_tree("Foo x;"), "(local Foo x)");
        assert_eq!(stmt_tree("Foo.Bar();"), "(expr (call (. Foo Bar)))");
        assert_eq!(stmt_tree("int.Parse(s);"), "(expr (call (. int Parse) s))");
        assert_eq!(stmt_tree("x = y;"), "(expr (= x y))");
    }

    #[test]
    fn return_if_and_while() {
        assert_eq!(stmt_tree("return;"), "(return)");
        assert_eq!(stmt_tree("return x + 1;"), "(return (+ x 1))");
        assert_eq!(stmt_tree("if (c) return;"), "(if c (return))");
        assert_eq!(
            stmt_tree("if (c) a(); else b();"),
            "(if c (expr (call a)) (expr (call b)))"
        );
        assert_eq!(
            stmt_tree("while (i < n) i++;"),
            "(while (< i n) (expr (post++ i)))"
        );
    }

    #[test]
    fn a_dangling_else_binds_to_the_nearest_if() {
        assert_eq!(
            stmt_tree("if (a) if (b) x(); else y();"),
            "(if a (if b (expr (call x)) (expr (call y))))"
        );
    }

    #[test]
    fn statement_diagnostics_match_the_reference_compiler() {
        assert_eq!(stmt_codes("f(x)"), vec![1002]);
        assert_eq!(stmt_codes("int x"), vec![1002]);
        assert_eq!(stmt_codes("{ f(x);"), vec![1513]);
    }

    #[test]
    fn loops_and_jumps() {
        assert_eq!(stmt_tree("do x(); while (c);"), "(do (expr (call x)) c)");
        assert_eq!(
            stmt_tree("for (int i = 0; i < n; i++) f();"),
            "(for (local int i=0) (< i n) (iters (post++ i)) (expr (call f)))"
        );
        assert_eq!(stmt_tree("for (;;) ;"), "(for _ _ _ (empty))");
        assert_eq!(
            stmt_tree("for (i = 0; ; i++, j--) {}"),
            "(for (exprs (= i 0)) _ (iters (post++ i) (post-- j)) (block))"
        );
        assert_eq!(
            stmt_tree("foreach (int x in xs) f(x);"),
            "(foreach int x xs (expr (call f x)))"
        );
        assert_eq!(stmt_tree("break;"), "(break)");
        assert_eq!(stmt_tree("continue;"), "(continue)");
        assert_eq!(stmt_tree("throw;"), "(throw)");
        assert_eq!(stmt_tree("throw new Error();"), "(throw (new Error))");
    }

    #[test]
    fn try_catch_finally() {
        assert_eq!(
            stmt_tree("try {} finally {}"),
            "(try (block) (finally (block)))"
        );
        assert_eq!(
            stmt_tree("try {} catch {}"),
            "(try (block) (catch (block)))"
        );
        assert_eq!(
            stmt_tree("try { a(); } catch (Exception e) { b(); }"),
            "(try (block (expr (call a))) (catch Exception e (block (expr (call b)))))"
        );
        assert_eq!(
            stmt_tree("try {} catch (A) {} catch (B b) {} finally {}"),
            "(try (block) (catch A (block)) (catch B b (block)) (finally (block)))"
        );
    }

    #[test]
    fn lock_using_and_checked_blocks() {
        assert_eq!(stmt_tree("lock (o) f();"), "(lock o (expr (call f)))");
        assert_eq!(stmt_tree("using (r) f();"), "(using r (expr (call f)))");
        assert_eq!(
            stmt_tree("using (Foo r = new Foo()) f();"),
            "(using (local Foo r=(new Foo)) (expr (call f)))"
        );
        assert_eq!(
            stmt_tree("checked { x(); }"),
            "(checked-block (block (expr (call x))))"
        );
        assert_eq!(
            stmt_tree("unchecked { y(); }"),
            "(unchecked-block (block (expr (call y))))"
        );
        assert_eq!(stmt_tree("checked(a + b);"), "(expr (checked (+ a b)))");
    }

    #[test]
    fn a_try_needs_a_catch_or_finally() {
        assert_eq!(stmt_codes("try {}"), vec![1524]);
        assert!(tokenize_ok("try {} catch {}"));
        assert!(tokenize_ok("try {} finally {}"));
    }

    fn tokenize_ok(source: &str) -> bool {
        parse_statement(source).diagnostics.is_empty()
    }

    #[test]
    fn switch_statements() {
        assert_eq!(stmt_tree("switch (x) {}"), "(switch x)");
        assert_eq!(
            stmt_tree("switch (x) { case 1: f(); break; default: g(); break; }"),
            "(switch x (section (case 1) (expr (call f)) (break)) (section (default) (expr (call g)) (break)))"
        );
        assert_eq!(
            stmt_tree("switch (x) { case 1: case 2: f(); break; }"),
            "(switch x (section (case 1) (case 2) (expr (call f)) (break)))"
        );
    }

    #[test]
    fn labeled_statements_and_goto() {
        assert_eq!(stmt_tree("done: ;"), "(label done (empty))");
        assert_eq!(
            stmt_tree("loop: while (c) break;"),
            "(label loop (while c (break)))"
        );
        assert_eq!(stmt_tree("goto done;"), "(goto done)");
        assert_eq!(stmt_tree("goto case 1;"), "(goto-case 1)");
        assert_eq!(stmt_tree("goto default;"), "(goto-default)");
        assert_eq!(stmt_tree("x;"), "(expr x)");
    }

    fn modifier_name(modifier: Modifier) -> &'static str {
        match modifier {
            Modifier::New => "new",
            Modifier::Public => "public",
            Modifier::Protected => "protected",
            Modifier::Internal => "internal",
            Modifier::Private => "private",
            Modifier::Abstract => "abstract",
            Modifier::Sealed => "sealed",
            Modifier::Static => "static",
            Modifier::Readonly => "readonly",
            Modifier::Volatile => "volatile",
            Modifier::Virtual => "virtual",
            Modifier::Override => "override",
            Modifier::Extern => "extern",
            Modifier::Const => "const",
            Modifier::Unsafe => "unsafe",
            Modifier::Required => "required",
            Modifier::Async => "async",
            Modifier::Partial => "partial",
            Modifier::Ref => "ref",
        }
    }

    fn dump_qname(name: &QualifiedName) -> String {
        let mut text = String::new();
        for (index, part) in name.parts.iter().enumerate() {
            if index > 0 {
                text.push('.');
            }
            text.push_str(part);
        }
        text
    }

    fn dump_using(directive: &UsingDirective) -> String {
        match &directive.kind {
            UsingKind::Namespace(name) => format!("(using {})", dump_qname(name)),
            UsingKind::Static(name) => format!("(using-static {})", dump_qname(name)),
            UsingKind::Alias { name, target } => {
                format!("(using-alias {name} {})", dump_qname(target))
            }
        }
    }

    fn dump_params(parameters: &[Parameter]) -> String {
        let mut text = String::new();
        for (index, parameter) in parameters.iter().enumerate() {
            if index > 0 {
                text.push_str(", ");
            }
            match parameter.modifier {
                Some(ParameterModifier::Ref) => text.push_str("ref "),
                Some(ParameterModifier::Out) => text.push_str("out "),
                Some(ParameterModifier::Params) => text.push_str("params "),
                None => {}
            }
            text.push_str(&format!("{} {}", dump_type(&parameter.ty), parameter.name));
        }
        text
    }

    fn vararg_marker(parameters: &[Parameter], is_vararg: bool) -> &'static str {
        match (is_vararg, parameters.is_empty()) {
            (false, _) => "",
            (true, true) => "__arglist",
            (true, false) => ", __arglist",
        }
    }

    fn dump_member(member: &Member) -> String {
        match member {
            Member::Field {
                modifiers,
                ty,
                declarators,
                ..
            } => {
                let mut text = String::from("(field");
                for modifier in modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                text.push_str(&format!(" {}", dump_type(ty)));
                for declarator in declarators {
                    match &declarator.initializer {
                        Some(value) => {
                            text.push_str(&format!(" {}={}", declarator.name, dump(value)));
                        }
                        None => text.push_str(&format!(" {}", declarator.name)),
                    }
                }
                text.push(')');
                text
            }
            Member::Method {
                modifiers,
                return_type,
                name,
                parameters,
                is_vararg,
                body,
                explicit_interface,
                ..
            } => {
                let mut text = String::from("(method");
                for modifier in modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                let qualified = match explicit_interface {
                    Some(interface) => format!("{}.{name}", dump_type(interface)),
                    None => name.to_string(),
                };
                text.push_str(&format!(
                    " {} {qualified} ({}{})",
                    dump_type(return_type),
                    dump_params(parameters),
                    vararg_marker(parameters, *is_vararg)
                ));
                match body {
                    Some(body) => text.push_str(&format!(" {}", dump_stmt(body))),
                    None => text.push_str(" ;"),
                }
                text.push(')');
                text
            }
            Member::Constructor {
                modifiers,
                name,
                parameters,
                is_vararg,
                initializer,
                body,
                ..
            } => {
                let mut text = String::from("(ctor");
                for modifier in modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                text.push_str(&format!(
                    " {name} ({}{})",
                    dump_params(parameters),
                    vararg_marker(parameters, *is_vararg)
                ));
                if let Some(initializer) = initializer {
                    let keyword = match initializer.kind {
                        ConstructorInitializerKind::Base => "base",
                        ConstructorInitializerKind::This => "this",
                    };
                    text.push_str(&format!(" :{keyword}("));
                    for (index, argument) in initializer.arguments.iter().enumerate() {
                        if index > 0 {
                            text.push(' ');
                        }
                        text.push_str(&dump_arg(argument));
                    }
                    text.push(')');
                }
                text.push_str(&format!(" {}", dump_stmt(body)));
                text.push(')');
                text
            }
            Member::Property {
                modifiers,
                ty,
                name,
                getter,
                setter,
                ..
            } => {
                let mut text = String::from("(property");
                for modifier in modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                text.push_str(&format!(" {} {name}", dump_type(ty)));
                if let Some(getter) = getter {
                    text.push_str(&format!(" {}", dump_accessor("get", getter)));
                }
                if let Some(setter) = setter {
                    let keyword = if setter.is_init { "init" } else { "set" };
                    text.push_str(&format!(" {}", dump_accessor(keyword, setter)));
                }
                text.push(')');
                text
            }
            Member::EventField {
                modifiers,
                ty,
                declarators,
                ..
            } => {
                let mut text = String::from("(event-field");
                for modifier in modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                text.push_str(&format!(" {}", dump_type(ty)));
                for declarator in declarators {
                    match &declarator.initializer {
                        Some(value) => {
                            text.push_str(&format!(" {}={}", declarator.name, dump(value)));
                        }
                        None => text.push_str(&format!(" {}", declarator.name)),
                    }
                }
                text.push(')');
                text
            }
            Member::Event {
                modifiers,
                ty,
                name,
                adder,
                remover,
                ..
            } => {
                let mut text = String::from("(event");
                for modifier in modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                text.push_str(&format!(" {} {name}", dump_type(ty)));
                if let Some(adder) = adder {
                    text.push_str(&format!(" {}", dump_accessor("add", adder)));
                }
                if let Some(remover) = remover {
                    text.push_str(&format!(" {}", dump_accessor("remove", remover)));
                }
                text.push(')');
                text
            }
            Member::Indexer {
                modifiers,
                ty,
                parameters,
                getter,
                setter,
                explicit_interface,
                ..
            } => {
                let mut text = String::from("(indexer");
                for modifier in modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                if let Some(interface) = explicit_interface {
                    text.push_str(&format!(" {}.this", dump_type(interface)));
                }
                text.push_str(&format!(" {} [{}]", dump_type(ty), dump_params(parameters)));
                if let Some(getter) = getter {
                    text.push_str(&format!(" {}", dump_accessor("get", getter)));
                }
                if let Some(setter) = setter {
                    let keyword = if setter.is_init { "init" } else { "set" };
                    text.push_str(&format!(" {}", dump_accessor(keyword, setter)));
                }
                text.push(')');
                text
            }
            Member::Operator {
                modifiers,
                return_type,
                operator,
                parameters,
                body,
                ..
            } => {
                let mut text = String::from("(operator");
                for modifier in modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                text.push_str(&format!(
                    " {} {} ({}) {}",
                    dump_type(return_type),
                    operator_symbol(*operator),
                    dump_params(parameters),
                    dump_stmt(body)
                ));
                text.push(')');
                text
            }
            Member::ConversionOperator {
                modifiers,
                direction,
                target,
                parameters,
                body,
                ..
            } => {
                let mut text = String::from("(conversion");
                for modifier in modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                let direction = match direction {
                    ConversionDirection::Implicit => "implicit",
                    ConversionDirection::Explicit => "explicit",
                };
                text.push_str(&format!(
                    " {direction} {} ({}) {}",
                    dump_type(target),
                    dump_params(parameters),
                    dump_stmt(body)
                ));
                text.push(')');
                text
            }
            Member::Destructor {
                modifiers,
                name,
                body,
                ..
            } => {
                let mut text = String::from("(dtor");
                for modifier in modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                text.push_str(&format!(" {name} {}", dump_stmt(body)));
                text.push(')');
                text
            }
            Member::NestedType(inner) => dump_namespace_member(inner),
            Member::Error => String::from("<error-member>"),
        }
    }

    fn operator_symbol(operator: OverloadableOperator) -> &'static str {
        match operator {
            OverloadableOperator::Plus => "+",
            OverloadableOperator::Minus => "-",
            OverloadableOperator::LogicalNot => "!",
            OverloadableOperator::BitwiseNot => "~",
            OverloadableOperator::Increment => "++",
            OverloadableOperator::Decrement => "--",
            OverloadableOperator::True => "true",
            OverloadableOperator::False => "false",
            OverloadableOperator::Multiply => "*",
            OverloadableOperator::Divide => "/",
            OverloadableOperator::Remainder => "%",
            OverloadableOperator::BitwiseAnd => "&",
            OverloadableOperator::BitwiseOr => "|",
            OverloadableOperator::ExclusiveOr => "^",
            OverloadableOperator::LeftShift => "<<",
            OverloadableOperator::RightShift => ">>",
            OverloadableOperator::Equality => "==",
            OverloadableOperator::Inequality => "!=",
            OverloadableOperator::GreaterThan => ">",
            OverloadableOperator::LessThan => "<",
            OverloadableOperator::GreaterThanOrEqual => ">=",
            OverloadableOperator::LessThanOrEqual => "<=",
        }
    }

    fn dump_type_decl(declaration: &TypeDecl) -> String {
        let keyword = match declaration.kind {
            TypeKind::Class => "class",
            TypeKind::Struct => "struct",
            TypeKind::Interface => "interface",
        };
        let mut text = match &declaration.record {
            None => format!("({keyword}"),
            Some(parts) if parts.keyword_form => format!("(record {keyword}"),
            Some(_) => String::from("(record"),
        };
        for modifier in &declaration.modifiers {
            text.push_str(&format!(" {}", modifier_name(*modifier)));
        }
        text.push_str(&format!(
            " {}{}",
            declaration.name,
            dump_type_parameters(&declaration.type_parameters, &declaration.constraints)
        ));
        if let Some(parts) = &declaration.record {
            if let Some(parameters) = &parts.parameters {
                text.push('(');
                for (index, parameter) in parameters.iter().enumerate() {
                    if index > 0 {
                        text.push_str(", ");
                    }
                    text.push_str(&format!("{} {}", dump_type(&parameter.ty), parameter.name));
                }
                text.push(')');
            }
        }
        if !declaration.bases.is_empty() {
            text.push_str(" :");
            for base in &declaration.bases {
                text.push_str(&format!(" {}", dump_type(base)));
            }
            if let Some(parts) = &declaration.record {
                if let Some(arguments) = &parts.base_arguments {
                    text.push('(');
                    for (index, argument) in arguments.iter().enumerate() {
                        if index > 0 {
                            text.push_str(", ");
                        }
                        text.push_str(&dump_arg(argument));
                    }
                    text.push(')');
                }
            }
        }
        for member in &declaration.members {
            text.push_str(&format!(" {}", dump_member(member)));
        }
        text.push(')');
        prefix_attributes(&declaration.attributes, text)
    }

    fn dump_attributes(sections: &[AttributeSection]) -> String {
        let mut text = String::new();
        for section in sections {
            text.push('[');
            if let Some(target) = &section.target {
                text.push_str(&format!("{target}: "));
            }
            for (index, attribute) in section.attributes.iter().enumerate() {
                if index > 0 {
                    text.push_str(", ");
                }
                text.push_str(&dump_qname(&attribute.name));
                if !attribute.arguments.is_empty() {
                    text.push('(');
                    for (argument_index, argument) in attribute.arguments.iter().enumerate() {
                        if argument_index > 0 {
                            text.push_str(", ");
                        }
                        match argument {
                            AttributeArgument::Positional(value) => text.push_str(&dump(value)),
                            AttributeArgument::Named { name, value } => {
                                text.push_str(&format!("{name}={}", dump(value)));
                            }
                        }
                    }
                    text.push(')');
                }
            }
            text.push(']');
        }
        text
    }

    /// `<T, R>` for a declaration that has type parameters, and the empty string otherwise --
    /// followed by ` where T : C` for each constraint clause.
    ///
    fn dump_type_parameters(
        parameters: &[TypeParameter],
        clauses: &[TypeParameterConstraintClause],
    ) -> String {
        let mut text = String::new();
        if !parameters.is_empty() {
            text.push('<');
            for (index, parameter) in parameters.iter().enumerate() {
                if index > 0 {
                    text.push_str(", ");
                }
                text.push_str(&parameter.name);
            }
            text.push('>');
        }
        for clause in clauses {
            text.push_str(&format!(" where {} :", clause.parameter));
            for (index, constraint) in clause.constraints.iter().enumerate() {
                if index > 0 {
                    text.push(',');
                }
                let rendered = match constraint {
                    TypeParameterConstraint::ReferenceType(_) => String::from("class"),
                    TypeParameterConstraint::ValueType(_) => String::from("struct"),
                    TypeParameterConstraint::DefaultConstructor(_) => String::from("new()"),
                    TypeParameterConstraint::Type(reference) => dump_type(reference),
                };
                text.push_str(&format!(" {rendered}"));
            }
        }
        text
    }

    fn prefix_attributes(sections: &[AttributeSection], body: String) -> String {
        let attributes = dump_attributes(sections);
        if attributes.is_empty() {
            body
        } else {
            format!("{attributes} {body}")
        }
    }

    fn dump_accessor(kind: &str, accessor: &Accessor) -> String {
        match &accessor.body {
            Some(body) => format!("({kind} {})", dump_stmt(body)),
            None => format!("({kind} ;)"),
        }
    }

    fn dump_namespace_member(member: &NamespaceMember) -> String {
        match member {
            NamespaceMember::Type(declaration) => dump_type_decl(declaration),
            NamespaceMember::Enum(declaration) => {
                let mut text = String::from("(enum");
                for modifier in &declaration.modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                text.push_str(&format!(" {}", declaration.name));
                if let Some(base) = &declaration.base {
                    text.push_str(&format!(" : {}", dump_type(base)));
                }
                for enum_member in &declaration.members {
                    match &enum_member.value {
                        Some(value) => {
                            text.push_str(&format!(" {}={}", enum_member.name, dump(value)));
                        }
                        None => text.push_str(&format!(" {}", enum_member.name)),
                    }
                }
                text.push(')');
                prefix_attributes(&declaration.attributes, text)
            }
            NamespaceMember::Delegate(declaration) => {
                let mut text = String::from("(delegate");
                for modifier in &declaration.modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                text.push_str(&format!(
                    " {} {}{} ({}){}",
                    dump_type(&declaration.return_type),
                    declaration.name,
                    dump_type_parameters(&declaration.type_parameters, &[]),
                    dump_params(&declaration.parameters),
                    dump_type_parameters(&[], &declaration.constraints)
                ));
                text.push(')');
                prefix_attributes(&declaration.attributes, text)
            }
            NamespaceMember::Namespace(declaration) => {
                let mut text = format!(
                    "(namespace{} {}",
                    if declaration.file_scoped { ";" } else { "" },
                    dump_qname(&declaration.name)
                );
                for using in &declaration.usings {
                    text.push_str(&format!(" {}", dump_using(using)));
                }
                for member in &declaration.members {
                    text.push_str(&format!(" {}", dump_namespace_member(member)));
                }
                text.push(')');
                text
            }
        }
    }

    fn dump_unit(unit: &CompilationUnit) -> String {
        let mut parts = String::new();
        let mut first = true;
        for using in &unit.usings {
            if !first {
                parts.push(' ');
            }
            first = false;
            parts.push_str(&dump_using(using));
        }
        for member in &unit.members {
            if !first {
                parts.push(' ');
            }
            first = false;
            parts.push_str(&dump_namespace_member(member));
        }
        parts
    }

    /// Renders an initializer so a test can see WHICH kind was chosen and how it nested -- the two
    /// things about this feature that can go wrong without drawing a diagnostic.
    fn dump_initializer(initializer: &Initializer) -> String {
        match initializer {
            Initializer::Object(members) => {
                let mut text = String::from("{obj");
                for member in members {
                    text.push_str(&format!(" {}=", member.name));
                    match &member.value {
                        MemberInitializerValue::Expression(value) => text.push_str(&dump(value)),
                        MemberInitializerValue::Nested(nested) => {
                            text.push_str(&dump_initializer(nested));
                        }
                    }
                }
                text.push('}');
                text
            }
            Initializer::Collection(elements) => {
                let mut text = String::from("{coll");
                for element in elements {
                    text.push(' ');
                    if element.arguments.len() == 1 {
                        text.push_str(&dump(&element.arguments[0]));
                    } else {
                        text.push('{');
                        for (index, argument) in element.arguments.iter().enumerate() {
                            if index > 0 {
                                text.push(' ');
                            }
                            text.push_str(&dump(argument));
                        }
                        text.push('}');
                    }
                }
                text.push('}');
                text
            }
        }
    }

    fn unit_tree(source: &str) -> String {
        let parsed = parse_compilation_unit(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "unexpected diagnostics for {source:?}: {:?}",
            parsed.diagnostics
        );
        dump_unit(&parsed.unit)
    }

    /// Expression bodies on the members that are NOT a method, property, indexer or property
    /// accessor. All five parse; the two GROUPS sit at different rungs, which is the part a single
    /// feature would have flattened.
    ///
    /// csc counts an expression-bodied OPERATOR as `expression-bodied method` and admits it at 6.0,
    /// while a constructor, static constructor and destructor are `expression body constructor and
    /// destructor` at 7.0. Measured, both directions.
    #[test]
    fn an_expression_body_is_accepted_on_every_member_that_takes_one() {
        assert!(unit_codes("public class P { public static P operator +(P a, P b) => a; }").is_empty());
        assert!(unit_codes("public class P { public static explicit operator int(P p) => 0; }").is_empty());
        assert!(unit_codes("public class P { int _v; public P(int v) => _v = v; }").is_empty());
        assert!(unit_codes("public class P { static int _n; static P() => _n = 1; }").is_empty());
        assert!(unit_codes("public class P { static int _n; ~P() => _n++; }").is_empty());
    }

    /// The two groups are gated a rung apart, so a build that admitted them together would accept
    /// a constructor at 6.0 that csc refuses.
    #[test]
    fn an_operator_body_is_a_rung_below_a_constructor_body() {
        let operator = "public class P { public static P operator +(P a, P b) => a; }";
        let constructor = "public class P { int _v; public P(int v) => _v = v; }";
        assert!(unit_codes_at(operator, LanguageVersion::CSharp6).is_empty());
        assert_eq!(
            unit_codes_at(constructor, LanguageVersion::CSharp6),
            vec![8059],
            "a constructor body is C# 7.0, and 8059 is the code for the rung in effect"
        );
        assert!(unit_codes_at(constructor, LanguageVersion::CSharp7).is_empty());
        assert_eq!(unit_codes_at(operator, LanguageVersion::CSharp5), vec![8026]);
    }

    /// A constructor and a destructor return nothing, so the body desugars to `{ e; }` and not to
    /// `{ return e; }`.
    ///
    /// **IT ASSERTS THE TREE, BECAUSE NO DIAGNOSTIC CAN SEE THIS.** Written first against
    /// `unit_codes` it read INERT under a perturbation that asked for a returned value: both forms
    /// parse, and the difference only shows in the shape. A parse-level test cannot check a
    /// parse-level choice by looking at whether anything complained.
    #[test]
    fn a_constructor_body_is_a_statement_expression_not_a_returned_value() {
        let ctor = unit_tree("public class P { int _v; public P(int v) => _v = v; }");
        assert!(
            !ctor.contains("return"),
            "a constructor body must not desugar to a return: {ctor}"
        );
        let dtor = unit_tree("public class P { static int _n; ~P() => _n++; }");
        assert!(
            !dtor.contains("return"),
            "a destructor body must not desugar to a return: {dtor}"
        );
        let method = unit_tree("public class P { public int Go() => 42; }");
        assert!(
            method.contains("return"),
            "the check cannot see a return at all: {method}"
        );
    }

    /// `out int a` DECLARES `a`; `out a` names one. The two share their first token, so the
    /// parser tells them apart by speculation and rolls the tokens back when the reading fails.
    ///
    #[test]
    fn an_out_argument_declares_a_variable_only_when_a_type_precedes_the_name() {
        assert_eq!(tree("M(out int a)"), "(call M (out (decl int a)))");
        assert_eq!(tree("M(out var a)"), "(call M (out (decl var a)))");
        assert_eq!(tree("M(out a)"), "(call M (out a))");
        assert_eq!(tree("M(out a.b)"), "(call M (out (. a b)))");
        assert_eq!(tree("M(ref a)"), "(call M (ref a))");
    }

    /// The declaration is C# 7.0; the `out` argument it rides on is C# 1.0.
    #[test]
    fn an_out_variable_declaration_is_csharp_7_and_a_plain_out_argument_is_not() {
        assert_eq!(codes_at("M(out int a)", LanguageVersion::CSharp6), vec![8059]);
        assert_eq!(codes_at("M(out var a)", LanguageVersion::CSharp6), vec![8059]);
        assert!(codes_at("M(out int a)", LanguageVersion::CSharp7).is_empty());
        assert!(codes_at("M(out a)", LanguageVersion::CSharp1).is_empty());
    }

    /// `x is T t` is a DECLARATION PATTERN and `x is T` is the C# 1.0 operator, so the designator
    /// alone carries the gate.
    ///
    #[test]
    fn a_designator_after_an_is_target_is_a_declaration_pattern() {
        assert_eq!(tree("o is string s"), "(is o string s)");
        assert_eq!(tree("o is int i"), "(is o int i)");
        assert_eq!(tree("o is string"), "(is o string)");
        assert_eq!(tree("o is int ? a : b"), "(?: (is o int) a b)");
        assert_eq!(tree("o is int? n"), "(is o int? n)");
    }

    /// The designator is C# 7.0; the operator it rides on is C# 1.0.
    #[test]
    fn a_declaration_pattern_is_csharp_7_and_the_is_operator_is_not() {
        assert_eq!(codes_at("o is string s", LanguageVersion::CSharp6), vec![8059]);
        assert!(codes_at("o is string s", LanguageVersion::CSharp7).is_empty());
        assert!(codes_at("o is string", LanguageVersion::CSharp1).is_empty());
    }

    /// `x is null` is a PATTERN and arrived at C# 7.0, a rung above the `is` operator it shares a
    /// keyword with. `x is T` is C# 1.0 and must stay so.
    #[test]
    fn a_null_pattern_is_csharp_7_and_a_type_test_is_not() {
        assert_eq!(codes_at("s is null", LanguageVersion::CSharp6), vec![8059]);
        assert!(codes_at("s is null", LanguageVersion::CSharp7).is_empty());
        assert!(codes_at("s is string", LanguageVersion::CSharp1).is_empty());
    }

    /// An element of a collection initializer is an ARGUMENT LIST, and a braced one hands `Add`
    /// several arguments at once.
    ///
    /// `{ { 1, 2 } }` is `Add(1, 2)`, which is the whole of how a dictionary initializer works --
    /// there is no dictionary construct in the language. The dump shows the braces so one call of
    /// two arguments is distinguishable from two calls of one, which is the confusion this shape
    /// exists to prevent.
    #[test]
    fn a_braced_collection_element_is_one_add_of_several_arguments() {
        assert_eq!(tree("new D { { 1, 2 } }"), "(new D {coll {1 2}})");
        assert_eq!(tree("new D { 1 }"), "(new D {coll 1})");
        assert_eq!(tree("new D { { 1 } }"), "(new D {coll 1})");
        assert_eq!(tree("new D { 1, { 2, 3 } }"), "(new D {coll 1 {2 3}})");
        assert_eq!(tree("new D { A = 1 }"), "(new D {obj A=1})");
    }

    fn unit_codes(source: &str) -> Vec<u16> {
        parse_compilation_unit(source)
            .diagnostics
            .iter()
            .map(Diagnostic::code)
            .collect()
    }

    /// Parses under `version` rather than the default dialect, and returns the tree dump.
    fn unit_tree_at(source: &str, version: LanguageVersion) -> String {
        let parsed = parse_compilation_unit_with(
            source,
            LexOptions {
                version,
                ..LexOptions::default()
            },
        );
        assert!(
            parsed.diagnostics.is_empty(),
            "unexpected diagnostics for {source:?}: {:?}",
            parsed.diagnostics
        );
        dump_unit(&parsed.unit)
    }

    /// The diagnostic codes `source` draws under `version`.
    /// **AN EXPRESSION BODY IS GATED AT ITS OWN MEMBER KIND, AND `=>` IS FOUR FEATURES.** csc
    /// names the declaration rather than the token -- measured at ISO-1, one declaration each:
    ///
    /// ```text
    /// int M() => 1;              'expression-bodied method'             C# 6.0
    /// int P => 1;                'expression-bodied property'           C# 6.0
    /// int this[int i] => 1;      'expression-bodied indexer'            C# 6.0
    /// int P { get => _v; }       'expression body property accessor'    C# 7.0
    /// x => x                     'lambda expression'                    C# 3.0
    /// ```
    ///
    /// The accessor's name is not spelled like the other three, and the indexer does not call
    /// itself a property. Both are csc's, copied rather than derived, because the text is a search
    /// key.
    ///
    /// The gate lives at these five parser sites rather than in the lexer, which cannot tell them
    /// apart: it named the lambda for all of them, and returned `Unknown`, so three of the five
    /// forms drew cascade diagnostics csc does not emit.
    #[test]
    fn an_expression_body_is_gated_at_its_own_member_kind() {
        let at = |version: LanguageVersion, source: &str| {
            let options = LexOptions { version, ..LexOptions::default() };
            let parsed = parse_compilation_unit_with(source, options);
            let mut names: Vec<String> = parsed
                .diagnostics
                .iter()
                .filter_map(|d| match &d.kind {
                    DiagnosticKind::FeatureRequiresLaterVersion { feature, .. } => {
                        Some(String::from(*feature))
                    }
                    _ => None,
                })
                .collect();
            names.sort();
            names.dedup();
            names
        };
        const METHOD: &str = "class C { public int M() => 1; }";
        const VOID: &str = "class C { void V() { } public void W() => V(); }";
        const PROPERTY: &str = "class C { public int P => 1; }";
        const INDEXER: &str = "class C { public int this[int i] => i; }";
        const ACCESSOR: &str = "class C { int _v; public int P { get => _v; } }";
        const LAMBDA: &str = "delegate int D(int x); class C { void M() { D d = x => x; } }";

        assert_eq!(at(LanguageVersion::CSharp1, METHOD), ["expression-bodied method"]);
        assert_eq!(at(LanguageVersion::CSharp1, VOID), ["expression-bodied method"]);
        assert_eq!(at(LanguageVersion::CSharp1, PROPERTY), ["expression-bodied property"]);
        assert_eq!(at(LanguageVersion::CSharp1, INDEXER), ["expression-bodied indexer"]);
        assert_eq!(at(LanguageVersion::CSharp1, ACCESSOR), ["expression body property accessor"]);
        assert_eq!(at(LanguageVersion::CSharp1, LAMBDA), ["lambda expression"]);

        for source in [METHOD, VOID, PROPERTY, INDEXER] {
            assert!(at(LanguageVersion::CSharp6, source).is_empty(), "{source} is C# 6");
        }
        assert_eq!(
            at(LanguageVersion::CSharp6, ACCESSOR),
            ["expression body property accessor"],
            "an accessor body is C# 7, one rung after the member forms"
        );
        assert!(at(LanguageVersion::CSharp7, ACCESSOR).is_empty());

        let body_of = |source: &str, member_name: &str| -> &'static str {
            let options = LexOptions {
                version: LanguageVersion::CSharp7,
                ..LexOptions::default()
            };
            let unit = parse_compilation_unit_with(source, options).unit;
            for member in unit.members.iter() {
                let NamespaceMember::Type(declaration) = member else { continue };
                for candidate in &declaration.members {
                    let body = match candidate {
                        Member::Method { name, body, .. } if &**name == member_name => body,
                        Member::Property { name, getter, .. } if &**name == member_name => {
                            &getter.as_ref().expect("a getter").body
                        }
                        _ => continue,
                    };
                    let Some(block) = body else { panic!("{member_name} has no body") };
                    let StmtKind::Block(statements) = &block.kind else {
                        panic!("an expression body desugars to a BLOCK")
                    };
                    return match statements.first().map(|s| &s.kind) {
                        Some(StmtKind::Return(Some(_))) => "return",
                        Some(StmtKind::Expression(_)) => "evaluate",
                        other => panic!("unexpected desugar: {other:?}"),
                    };
                }
            }
            panic!("no member named {member_name}")
        };
        assert_eq!(body_of(METHOD, "M"), "return", "an int method returns its expression");
        assert_eq!(body_of(VOID, "W"), "evaluate", "a VOID method evaluates it -- `return e;` is CS0127");
        assert_eq!(body_of(PROPERTY, "P"), "return", "a property getter returns its expression");
        assert_eq!(
            body_of("class C { int _v; public int P { get => _v; set => _v = value; } }", "P"),
            "return",
            "and so does a get accessor"
        );

        assert!(
            at(LanguageVersion::CSharp7, LAMBDA).is_empty(),
            "C# 7 HAS lambdas; what this dialect permits and does not lower is the binder's to report"
        );
    }

    /// The tree dump at `version`, IGNORING diagnostics.
    ///
    /// **FOR A CONSTRUCT WHOSE GATE IS THE POINT.** `unit_tree_at` requires a clean parse, which a
    /// feature that is parsed-but-not-implemented can never give -- the gate fires at every
    /// version by design, so that the binder can name the construct rather than cascade. This
    /// asserts the SHAPE, which the parser produces at every version; the gate itself is asserted
    /// separately by `unit_codes_at`, and asserting both through one helper would mean asserting
    /// neither.
    fn unit_tree_ignoring_gates(source: &str, version: LanguageVersion) -> String {
        let parsed = parse_compilation_unit_with(
            source,
            LexOptions {
                version,
                ..LexOptions::default()
            },
        );
        dump_unit(&parsed.unit)
    }

    /// The dump with a record's VALUE-EQUALITY GROUP elided, so a test about the DECLARATION reads
    /// as one.
    ///
    /// **THE GROUP IS PROVEN ELSEWHERE AND BY A BETTER ORACLE.** `tools/record-members.ps1`
    /// compares what lcsc and csc each WROTE, member by member and flag by flag, and
    /// `tools/record-runtime.ps1` compares what they DO against .NET. Pasting the group's tree into
    /// nine assertions here would add two thousand characters that say only "the synthesis did what
    /// this file's synthesis does" -- a test mirroring its implementation -- while making the
    /// positional list, the base list and the user-declared-member rule unreadable.
    ///
    /// The elision is ANCHORED on the group's first member, so a synthesis that stopped emitting it
    /// fails these tests rather than quietly leaving them green.
    fn unit_tree_declaration(source: &str, version: LanguageVersion) -> String {
        let dumped = unit_tree_ignoring_gates(source, version);
        const ANCHOR: &str = " (property protected virtual System.Type EqualityContract";
        match dumped.find(ANCHOR) {
            Some(at) => {
                let closers = dumped[at..].matches(')').count() - dumped[at..].matches('(').count();
                alloc::format!("{}{}", &dumped[..at], ")".repeat(closers))
            }
            None => dumped,
        }
    }

    fn unit_codes_at(source: &str, version: LanguageVersion) -> Vec<u16> {
        parse_compilation_unit_with(
            source,
            LexOptions {
                version,
                ..LexOptions::default()
            },
        )
        .diagnostics
        .iter()
        .map(Diagnostic::code)
        .collect()
    }

    #[test]
    fn an_initializer_parses_into_the_tree() {
        let v3 = LanguageVersion::CSharp3;
        assert_eq!(
            tree_at("new C { F = 1, G = 2 }", v3),
            "(new C {obj F=1 G=2})"
        );
        assert_eq!(tree_at("new C { 1, 2 }", v3), "(new C {coll 1 2})");
        assert_eq!(tree_at("new C { }", v3), "(new C {obj})");
        assert_eq!(tree_at("new C { F = 1, }", v3), "(new C {obj F=1})");
        assert_eq!(tree_at("new C { 1, }", v3), "(new C {coll 1})");
        assert_eq!(tree_at("new C(1) { F = 2 }", v3), "(new C 1 {obj F=2})");
        assert_eq!(tree_at("new C(1)", v3), "(new C 1)");

        assert_eq!(
            tree_at("new C { F = { G = 1 } }", v3),
            "(new C {obj F={obj G=1}})"
        );
    }

    #[test]
    fn required_is_a_modifier_or_an_identifier_depending_on_the_dialect() {
        let v10 = LanguageVersion::CSharp10;
        let v11 = LanguageVersion::CSharp11;

        assert_eq!(unit_codes_at("class C { required int f; }", v10), [8936]);
        assert_eq!(unit_codes_at("class C { required int f; }", v11), []);
        assert_eq!(unit_codes_at("class C { required Foo f; }", v10), [8936]);

        assert_eq!(unit_codes_at("class C { required f; }", v10), []);
        assert_ne!(unit_codes_at("class C { required f; }", v11), []);

        assert_eq!(unit_codes_at("class C { int required = 5; }", v10), []);
        assert_eq!(unit_codes_at("class C { int required = 5; }", v11), []);
        assert_eq!(unit_codes_at("class C { void M(int required) { } }", v11), []);

        assert_eq!(unit_codes("class C { required int f; }"), []);
        assert_eq!(unit_codes_at("class C { required int f; }", LanguageVersion::CSharp1), [8022]);
    }

    #[test]
    fn async_is_a_modifier_exactly_where_csc_reads_one() {
        let v4 = LanguageVersion::CSharp4;
        let v5 = LanguageVersion::CSharp5;

        assert_eq!(unit_codes_at("class C { static async void M() { } }", v5), []);
        assert_eq!(unit_codes_at("class C { static async void M() { } }", v4), [8025]);
        assert_eq!(unit_codes_at("class C { static async void M() { } }", LanguageVersion::CSharp1), [8022]);
        assert_eq!(
            unit_codes_at("class C { static async void M() { await this.T(); } }", v4),
            [8025]
        );

        assert_eq!(unit_codes_at("class async { } class C { async f; }", v5), []);
        assert_eq!(
            unit_codes_at("class async { } class C { async async() { return new async(); } }", v5),
            []
        );
        assert_eq!(unit_codes_at("class C { async C() { } }", v5), []);
        assert_eq!(
            unit_codes_at("class async { } class C { async async async() { return null; } }", v5),
            []
        );
        assert_eq!(
            unit_tree_at("class async { } class C { async async async() { return null; } }", v5),
            "(class async) (class C (method async async async () (block (return null))))"
        );
        assert_eq!(
            unit_codes_at(
                "using System.Threading.Tasks; class C { static async Task<int> M() { return 1; } }",
                v5
            ),
            []
        );

        assert_eq!(
            unit_codes_at("abstract class C { public abstract async void M(); }", v5),
            [1994]
        );

        assert_eq!(
            unit_codes_at(
                "class C { static void N() { int async = 3; int await = 4; async = await; } }",
                v5
            ),
            []
        );
    }

    #[test]
    fn await_is_reserved_inside_an_async_method_and_ordinary_outside() {
        let v4 = LanguageVersion::CSharp4;
        let v5 = LanguageVersion::CSharp5;

        assert_eq!(
            unit_tree_at(
                "class C { object x; async void M() { await this.x; } }",
                v5
            ),
            "(class C (field object x) (method async void M () (block (expr (await (. this x))))))"
        );
        assert_eq!(
            unit_codes_at("class C { async void M() { int await = 4; } }", v5),
            [4003]
        );
        assert_eq!(
            unit_codes_at("class C { async void M(int await) { } }", v5),
            [4003]
        );
        assert_eq!(
            unit_codes_at("class C { async void M() { int @await = 4; @await = 5; } }", v5),
            []
        );

        assert_eq!(
            unit_codes_at("class C { static object T() { return null; } static void N() { await T(); } }", v5),
            [4033]
        );
        assert_eq!(
            unit_codes_at(
                "class C { static object T() { return null; } static void N() { object x = await T(); } }",
                v5
            ),
            [4033]
        );
        assert_eq!(
            unit_codes_at("class C { static object T() { return null; } static void N() { await T(); } }", v4),
            [8025, 4033]
        );
        assert_eq!(
            unit_codes_at("class C { static void N() { object x = await new object(); } }", v5),
            [4033]
        );

        assert_eq!(
            unit_codes_at(
                "class C { object d; async void M() { unsafe { await this.d; } } }",
                v5
            ),
            [4004]
        );
        assert_eq!(
            unit_codes_at(
                "class C { object d; async void M() { unsafe { } await this.d; } }",
                v5
            ),
            []
        );

        assert_eq!(
            unit_codes_at(
                "class C { static int await(int x) { return x; } static void N() { int y = await(1); } }",
                v5
            ),
            []
        );
        assert_eq!(
            unit_codes_at("class C { static void N() { int await = 3; int y = await + 1; } }", v5),
            []
        );
        assert_eq!(
            unit_codes_at(
                "class C { static void N() { int[] await = new int[1]; int y = await[0]; } }",
                v5
            ),
            []
        );
        assert_eq!(
            unit_codes_at("class C { static void N() { int await = 3; int y = await; } }", v5),
            []
        );
        assert_eq!(unit_codes_at("class await { } class C { await f; }", v5), []);
    }

    #[test]
    fn a_file_scoped_namespace_holds_everything_after_it() {
        assert_eq!(
            unit_tree_at("namespace N; class C { }", LanguageVersion::CSharp10),
            "(namespace; N (class C))"
        );
        assert_eq!(
            unit_tree_at("namespace A.B.C; class D { }", LanguageVersion::CSharp10),
            "(namespace; A.B.C (class D))"
        );
        assert_eq!(
            unit_tree_at(
                "using System; namespace N; using System.Text; class C { }",
                LanguageVersion::CSharp10
            ),
            "(using System) (namespace; N (using System.Text) (class C))"
        );
        assert_eq!(
            unit_tree_at("namespace N;", LanguageVersion::CSharp10),
            "(namespace; N)"
        );
        assert_eq!(
            unit_tree_at("namespace N; class C { } class D { }", LanguageVersion::CSharp10),
            "(namespace; N (class C) (class D))"
        );
        assert_eq!(
            unit_tree_at("namespace N { class C { } }", LanguageVersion::CSharp10),
            "(namespace N (class C))"
        );
    }

    #[test]
    fn a_file_scoped_namespace_is_gated_below_csharp_10() {
        assert_eq!(
            unit_codes_at("namespace N; class C { }", LanguageVersion::CSharp1),
            [8022]
        );
        assert_eq!(
            unit_codes_at("namespace N; class C { }", LanguageVersion::CSharp9),
            [8773]
        );
        assert_eq!(
            unit_codes_at("namespace N; class C { }", LanguageVersion::CSharp10),
            []
        );
        assert_eq!(unit_codes("namespace N; class C { }"), []);
        assert_eq!(
            unit_codes_at("namespace N; class C { }", LanguageVersion::CSharp9),
            [8773]
        );
    }

    #[test]
    fn the_three_file_scoped_namespace_placement_rules() {
        let at10 = LanguageVersion::CSharp10;

        assert_eq!(unit_codes_at("class D { } namespace N;", at10), [8956]);
        assert_eq!(unit_codes_at("namespace M { } namespace N;", at10), [8956]);
        assert_eq!(unit_codes_at("delegate void D(); namespace N;", at10), [8956]);
        assert_eq!(unit_codes_at("using System; namespace N;", at10), []);
        assert_eq!(unit_codes_at("[assembly: A] namespace N;", at10), []);

        assert_eq!(unit_codes_at("namespace N; namespace M { }", at10), [8955]);
        assert_eq!(unit_codes_at("namespace M { namespace N; }", at10), [8955]);
        assert_eq!(
            unit_codes_at("namespace N; namespace M { } namespace O { }", at10),
            [8955, 8955]
        );
        assert_eq!(
            unit_codes_at("namespace N; class C { } namespace M { }", at10),
            [8955]
        );

        assert_eq!(unit_codes_at("namespace N; namespace M;", at10), [8954]);
        assert_eq!(
            unit_codes_at("namespace N; namespace M; namespace O;", at10),
            [8954, 8954]
        );

        assert_eq!(
            unit_codes_at("namespace Z { } namespace N; namespace M;", at10),
            [8956, 8954]
        );

        assert_eq!(
            unit_codes_at("namespace M { namespace N; namespace O; }", at10),
            [8955, 8954]
        );
        assert_eq!(
            unit_codes_at("namespace N; namespace M { namespace O; }", at10),
            [8955, 8955]
        );

        assert_eq!(
            unit_codes_at("class D { } namespace N;", LanguageVersion::CSharp1),
            [8022, 8956]
        );
    }

    #[test]
    fn a_namespace_whose_name_did_not_parse_is_not_classified() {
        assert_eq!(unit_codes_at("namespace ;", LanguageVersion::CSharp1), [1001]);
        assert_eq!(
            unit_codes_at("class D { } namespace ;", LanguageVersion::CSharp10),
            [1001]
        );
        assert_eq!(
            unit_codes_at("namespace M { namespace ; }", LanguageVersion::CSharp10),
            [1001]
        );
        assert_eq!(
            unit_codes_at("class D { } namespace N;", LanguageVersion::CSharp10),
            [8956]
        );
    }

    #[test]
    fn global_assembly_and_module_attributes_parse_and_collect() {
        assert_eq!(unit_codes("[assembly: Foo(\"x\")] class C {}"), []);
        let parsed = parse_compilation_unit("[assembly: A] [module: B] class C {} class D {}");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(parsed.unit.members.len(), 2);
        assert_eq!(parsed.unit.global_attributes.len(), 2);
        assert_eq!(parsed.unit.global_attributes[0].target.as_deref(), Some("assembly"));
        assert_eq!(parsed.unit.global_attributes[1].target.as_deref(), Some("module"));
        let parsed = parse_compilation_unit("[Serializable] class C {}");
        assert!(parsed.unit.global_attributes.is_empty());
        assert_eq!(parsed.unit.members.len(), 1);
    }

    #[test]
    fn a_global_attribute_after_a_member_reports_cs1730() {
        assert_eq!(unit_codes("class C {} [assembly: A]"), [1730]);
        assert_eq!(unit_codes("namespace N {} [assembly: A]"), [1730]);
        assert_eq!(unit_codes("class C {} [module: B]"), [1730]);

        assert_eq!(unit_codes("[assembly: A] class C {}"), []);
        assert_eq!(unit_codes("using System; [assembly: A] class C {}"), []);
    }

    #[test]
    fn a_namespace_with_modifiers_or_attributes_reports_cs1671() {
        assert_eq!(unit_codes("[A] namespace N { }"), [1671]);
        assert_eq!(unit_codes("public namespace N { }"), [1671]);
        assert_eq!(unit_codes("[A] public namespace N { }"), [1671, 1671]);

        assert_eq!(unit_codes("namespace N { }"), []);
        assert_eq!(unit_codes("[A] class C { }"), []);
        assert_eq!(unit_codes("public class C { }"), []);
    }

    #[test]
    fn a_generic_declaration_reports_one_feature_code_not_a_parse_cascade() {
        assert_eq!(unit_codes_at("public class C<T> { }", LanguageVersion::CSharp1), [8022]);
        assert_eq!(unit_codes_at("public class E { public T M<T>(T x) { return x; } }", LanguageVersion::CSharp1), [8022]);
        assert_eq!(unit_codes_at("class D { System.Collections.Generic.List<int> f; }", LanguageVersion::CSharp1), [8022]);

        assert_eq!(unit_codes_at("public class C<T> : B { void M() { } int F; }", LanguageVersion::CSharp1), [8022]);
        assert_eq!(unit_codes_at("public class C<T, U> { }", LanguageVersion::CSharp1), [8022]);
        assert_eq!(unit_codes_at("class D { System.Collections.Generic.List<System.Collections.Generic.List<int>> f; }", LanguageVersion::CSharp1), [8022]);

        assert_eq!(unit_codes("public class C : B { void M() { } int F; }"), []);
        assert_eq!(unit_codes("public class C { public int M(int x) { return x; } }"), []);
        assert_eq!(unit_codes("public class C { public int F = 1; }"), []);
    }

    #[test]
    fn a_dialect_with_generics_parses_the_type_parameter_list_instead_of_skipping_it() {
        assert_eq!(unit_codes_at("public class C<T> { }", LanguageVersion::CSharp2), []);
        assert_eq!(
            unit_codes_at(
                "public class E { public int M<T>(int x) { return x; } }",
                LanguageVersion::CSharp2
            ),
            []
        );
        assert_eq!(unit_codes_at("public class C<T, U> { }", LanguageVersion::CSharp2), []);

        assert_eq!(unit_codes_at("public class C<T> { }", LanguageVersion::CSharp1), [8022]);
        assert_eq!(unit_codes_at("public class C<T> { }", LanguageVersion::CSharp1), [8022]);

        let parsed = parse_compilation_unit_with(
            "public class Box<T, U> { }",
            LexOptions {
                version: LanguageVersion::CSharp2,
                ..LexOptions::default()
            },
        );
        let NamespaceMember::Type(declaration) = &parsed.unit.members[0] else {
            panic!("expected a type declaration");
        };
        let names: Vec<&str> = declaration
            .type_parameters
            .iter()
            .map(|parameter| &*parameter.name)
            .collect();
        assert_eq!(names, ["T", "U"]);

        let parsed = parse_compilation_unit_with(
            "public class Box { }",
            LexOptions {
                version: LanguageVersion::CSharp2,
                ..LexOptions::default()
            },
        );
        let NamespaceMember::Type(declaration) = &parsed.unit.members[0] else {
            panic!("expected a type declaration");
        };
        assert!(declaration.type_parameters.is_empty());
    }

    /// The declared type of the single field in `class C { ... }`, parsed under `version`.
    fn field_type_at(source: &str, version: LanguageVersion) -> String {
        let parsed = parse_compilation_unit_with(
            source,
            LexOptions {
                version,
                ..LexOptions::default()
            },
        );
        let NamespaceMember::Type(declaration) = &parsed.unit.members[0] else {
            panic!("expected a type declaration");
        };
        let Member::Field { ty, .. } = &declaration.members[0] else {
            panic!("expected a field");
        };
        dump_type(ty)
    }

    /// `typeof(List<>)` -- the UNBOUND generic type (ECMA-334 4th ed 14.5.11). The arity is the
    /// `generic-dimension-specifier`'s comma count plus one, and it reaches the tree, so an
    /// assertion here can fail: a dump printing `(typeof List)` either way would pass against a
    /// parser that discarded the specifier and left `List` behind.
    #[test]
    fn an_unbound_generic_type_is_a_typeof_operand_and_its_arity_is_the_comma_count_plus_one() {
        let v2 = LanguageVersion::CSharp2;

        assert_eq!(tree_at("typeof(List<>)", v2), "(typeof List<>)");
        assert_eq!(tree_at("typeof(Dictionary<,>)", v2), "(typeof Dictionary<,>)");
        assert_eq!(tree_at("typeof(A<,,>)", v2), "(typeof A<,,>)");
        assert_eq!(
            tree_at("typeof(System.Collections.Generic.List<>)", v2),
            "(typeof System.Collections.Generic.List<>)"
        );

        assert_eq!(tree_at("typeof(int)", v2), "(typeof int)");
        assert_eq!(tree_at("typeof(List)", v2), "(typeof List)");
        assert_eq!(tree_at("typeof(List<int>)", v2), "(typeof List<int>)");
        assert_eq!(tree_at("typeof(int[])", v2), "(typeof int[])");
    }

    /// The unbound form is legal in the `typeof` operand and NOWHERE else, so every other position
    /// still refuses it -- which is the property that lets the parser scope the grammar to one
    /// production instead of teaching ~20 type positions to say no.
    ///
    #[test]
    fn an_unbound_generic_name_outside_typeof_is_refused() {
        let v2 = LanguageVersion::CSharp2;

        assert!(!unit_codes_at("class P { void M() { List<> x = null; } }", v2).is_empty());
        assert!(!unit_codes_at("class P { List<> f; }", v2).is_empty());
        assert!(!unit_codes_at("class P { void M(List<> x) { } }", v2).is_empty());
        assert!(!unit_codes_at("class P : List<> { }", v2).is_empty());
        assert!(!unit_codes_at("class P { void M(object o) { var x = (List<>)o; } }", v2).is_empty());
        assert!(!unit_codes_at("class P { void M() { int n = sizeof(List<>); } }", v2).is_empty());
        assert!(!unit_codes_at("class P { void M() { var t = typeof(List<>[]); } }", v2).is_empty());
    }

    /// Below C# 2 the unbound form draws ONE feature diagnostic, and it is the SAME one
    /// `typeof(List<int>)` draws from the same line of code -- [`parse_unbound_type_name`] declines
    /// rather than reporting, so the gate keeps a single implementation.
    #[test]
    fn an_unbound_generic_type_gates_as_generics_below_csharp_2() {
        assert_eq!(
            unit_codes_at(
                "class P { void M() { var t = typeof(List<>); } }",
                LanguageVersion::CSharp1
            ),
            unit_codes_at(
                "class P { void M() { var t = typeof(List<int>); } }",
                LanguageVersion::CSharp1
            )
        );
        assert!(
            unit_codes_at(
                "class P { void M() { var t = typeof(List<>); } }",
                LanguageVersion::CSharp1
            )
            .contains(&8022)
        );
    }

    #[test]
    fn a_generic_call_site_is_told_from_two_comparisons_by_what_follows_the_angle() {
        let v2 = LanguageVersion::CSharp2;

        assert_eq!(tree_at("Identity<int>(x)", v2), "(call<int> Identity x)");
        assert_eq!(
            tree_at("Map<int,string>(a,b)", v2),
            "(call<int,string> Map a b)"
        );
        assert_eq!(
            tree_at("c.Identity<int>(x)", v2),
            "(call<int> (. c Identity) x)"
        );

        assert_eq!(tree_at("a < b > (c)", v2), "(call<b> a c)");

        assert!(
            !tree_at("F(G<A, B>7)", v2).contains("call<"),
            "`>7` is not in the follower set, so G<A,B> is two comparisons"
        );
        assert!(
            !tree_at("F<A> + y", v2).contains("call<"),
            "9.2.3: `x = F<A> + y` is less-than, greater-than and unary-plus"
        );
        assert!(!tree_at("a < b", v2).contains("call<"));
        assert!(!tree_at("a < b && c > d", v2).contains("call<"));

        assert!(
            !tree_at("a < (b >> c)", v2).contains("call<"),
            "a failed type-argument parse must not be read as a call site"
        );

        let v1 = LanguageVersion::CSharp1;
        assert!(!tree_at("a < b > (c)", v1).contains("call<"));
    }

    #[test]
    fn a_constructed_type_is_told_from_two_comparisons_by_the_dot_that_follows() {
        let v2 = LanguageVersion::CSharp2;

        assert_eq!(tree_at("Box<int>.Count", v2), "(. (ctype Box 1) Count)");
        assert_eq!(
            tree_at("Pair<int,string>.Count", v2),
            "(. (ctype Pair 2) Count)"
        );
        assert_eq!(tree_at("N.Box<int>.Count", v2), "(. (ctype (. N Box) 1) Count)");

        assert!(!tree_at("a < b > .5", v2).contains("ctype"));
        assert!(!tree_at("a < b > s.V", v2).contains("ctype"));
        assert_eq!(tree_at("Identity<int>(x)", v2), "(call<int> Identity x)");
        assert!(!tree_at("a < b", v2).contains("ctype"));

        assert_eq!(
            unit_codes_at(
                "class C { void M() { int n = Box<int>.Count; } }",
                LanguageVersion::CSharp1
            ),
            [1525]
        );
    }

    #[test]
    fn a_failed_generic_call_speculation_leaves_no_diagnostics_behind() {
        let v2 = LanguageVersion::CSharp2;
        assert_eq!(unit_codes_at("class C { void M() { int x = a < b > (c); } }", v2), []);
        assert_eq!(unit_codes_at("class C { void M() { bool y = p < q; } }", v2), []);
    }

    #[test]
    fn a_dialect_with_generics_parses_a_use_site_into_a_constructed_type() {
        let v2 = LanguageVersion::CSharp2;

        assert_eq!(unit_codes_at("class C { Box<int> f; }", v2), []);
        assert_eq!(field_type_at("class C { Box<int> f; }", v2), "Box<int>");
        assert_eq!(
            field_type_at("class C { System.Collections.Generic.List<int> f; }", v2),
            "System.Collections.Generic.List<int>"
        );
        assert_eq!(
            field_type_at("class C { Dictionary<string,int> f; }", v2),
            "Dictionary<string,int>"
        );

        assert_eq!(field_type_at("class C { Box<int[]> f; }", v2), "Box<int[]>");
        assert_eq!(field_type_at("class C { Box<int>[] f; }", v2), "Box<int>[]");
        assert_eq!(field_type_at("class C { Box<A.B> f; }", v2), "Box<A.B>");

        assert_eq!(unit_codes_at("class C { Box<int> f; }", LanguageVersion::CSharp1), [8022]);
        assert_eq!(field_type_at("class C { Box<int> f; }", LanguageVersion::CSharp1), "Box");
    }

    #[test]
    fn a_type_argument_list_sits_on_the_part_it_was_written_on() {
        let v2 = LanguageVersion::CSharp2;

        assert_eq!(unit_codes_at("class C { List<int>.Enumerator f; }", v2), []);
        assert_eq!(
            field_type_at("class C { List<int>.Enumerator f; }", v2),
            "List<int>.Enumerator"
        );
        assert_eq!(
            field_type_at("class C { Box<int>.Pair<string> f; }", v2),
            "Box<int>.Pair<string>"
        );
        assert_eq!(
            field_type_at(
                "class C { System.Collections.Generic.List<int>.Enumerator f; }",
                v2
            ),
            "System.Collections.Generic.List<int>.Enumerator"
        );
        assert_eq!(
            field_type_at("class C { Box<int>.Ring.Hub f; }", v2),
            "Box<int>.Ring.Hub"
        );
        assert_eq!(
            field_type_at("class C { Box<int>.Cursor[] f; }", v2),
            "Box<int>.Cursor[]"
        );
        assert_eq!(field_type_at("class C { A.B.C f; }", v2), "A.B.C");

        assert_eq!(
            field_type_at("class C { Box<Box<int>>.Cursor f; }", v2),
            "Box<Box<int>>.Cursor"
        );
        assert_eq!(unit_codes_at("class C { Box<Box<int>>.Cursor f; }", v2), []);

        assert_eq!(unit_codes_at("class C { List<int>.Enumerator f; }", LanguageVersion::CSharp1), [8022]);
        assert_eq!(
            field_type_at("class C { List<int>.Enumerator f; }", LanguageVersion::CSharp1),
            "List.Enumerator"
        );
        assert_eq!(unit_codes_at("class C { Box<int>.Pair<string> f; }", LanguageVersion::CSharp1), [8022, 8022]);
        assert_eq!(unit_codes_at("class C { A.B<int> f; }", LanguageVersion::CSharp1), [8022]);
    }

    #[test]
    fn a_nested_type_argument_list_closes_on_the_right_shift_token() {
        let v2 = LanguageVersion::CSharp2;

        assert_eq!(unit_codes_at("class C { Box<Box<int>> f; }", v2), []);
        assert_eq!(
            field_type_at("class C { Box<Box<int>> f; }", v2),
            "Box<Box<int>>"
        );
        assert_eq!(
            field_type_at("class C { Box<Box<Box<int>>> f; }", v2),
            "Box<Box<Box<int>>>"
        );
        assert_eq!(
            field_type_at("class C { Map<Box<int>,int> f; }", v2),
            "Map<Box<int>,int>"
        );
        assert_eq!(
            field_type_at("class C { Map<int,Box<int>> f; }", v2),
            "Map<int,Box<int>>"
        );
        assert_eq!(
            field_type_at("class C { Box<Box<int>>[] f; }", v2),
            "Box<Box<int>>[]"
        );
    }

    #[test]
    fn a_right_shift_survives_the_local_declaration_speculation() {
        let v2 = LanguageVersion::CSharp2;

        assert_eq!(
            unit_tree_at("class C { void M() { a<b>>c; } }", v2),
            "(class C (method void M () (block (expr (< a (>> b c))))))"
        );
        assert_eq!(unit_codes_at("class C { void M() { a<b>>c; } }", LanguageVersion::CSharp1), [8022]);

        assert_eq!(
            unit_tree_at("class C { void M() { a<b<c>> d; } }", v2),
            "(class C (method void M () (block (local a<b<c>> d))))"
        );
    }

    #[test]
    fn post_1_0_features_report_cs8022() {
        let unit_codes = |source: &str| unit_codes_at(source, LanguageVersion::CSharp1);
        assert_eq!(unit_codes("class C { System.Collections.Generic.List<int> f; }"), [8022]);
        assert_eq!(unit_codes("class C : System.Collections.Generic.List<int> { }"), [8022]);
        assert_eq!(
            unit_codes(
                "class C { System.Collections.Generic.List<System.Collections.Generic.List<int>> f; }"
            ),
            [8022]
        );
        assert_eq!(unit_codes("class C { object M(object a) { return a ?? a; } }"), [8022]);
        assert_eq!(unit_codes("class C { void M() { int x = A::B; } }"), [8022]);
        assert_eq!(unit_codes("class C { void M(string a) { a?.ToString(); } }"), [8022]);
        assert_eq!(unit_codes("class C { int P; int Q => 5; }"), [8022]);
        assert!(unit_codes("class C { void M() { System.Func<int,int> f = x => x; } }").contains(&8022));
        assert_eq!(unit_codes("class C { void M() { D d = delegate { }; } }"), [8022]);
        assert_eq!(
            unit_codes("class C { void M() { D d = delegate (int x) { return x + 1; }; } }"),
            [8022]
        );
        assert_eq!(unit_codes("using static System.Math; class C { }"), [8022]);
        assert_eq!(unit_codes("class C { C M() { return new C() { F = 1 }; } }"), [8022]);
        assert_eq!(unit_codes("class C { C M() { return new C { F = 1 }; } }"), [8022]);
        assert_eq!(unit_codes("class C { object M() { return new { A = 1 }; } }"), [8022]);
        assert_eq!(unit_codes("class C { void M(int x = 5) { } }"), [8022]);
        assert_eq!(unit_codes("class C { void M(int x) { } void N() { M(x: 5); } }"), [8022]);
        assert_eq!(
            unit_codes(
                "class C { int f; public int P { get { return f; } private set { f = value; } } }"
            ),
            []
        );
        assert_eq!(unit_codes("class C { int? f; }"), [8022]);
        assert_eq!(unit_codes("class C { int? M() { return 0; } }"), [8022]);
        assert_eq!(unit_codes("class C { void M(int? x) { } }"), [8022]);
        assert_eq!(unit_codes("class C { object M(int x) { return (int?)x; } }"), [8022]);
        assert_eq!(unit_codes("class C { System.Type M() { return typeof(int?); } }"), [8022]);
        assert!(
            !unit_codes("class C { bool M(object x, bool a, bool b) { return x is int ? a : b; } }")
                .contains(&8022)
        );
        assert!(
            !unit_codes("class C { int M(bool Foo, int a, int c) { return Foo ? a : c; } }")
                .contains(&8022)
        );
        assert!(!unit_codes("class C { void M() { int[] a = new int[] { 1, 2 }; } }").contains(&8022));
        assert!(!unit_codes("class C { int F() { return 1; } }").contains(&8022));
    }

    #[test]
    fn a_nullable_array_type_is_not_the_null_conditional_indexer() {
        assert_eq!(tree_at("typeof(int?[])", LanguageVersion::CSharp6), "(typeof int?[])");
        assert_eq!(tree_at("typeof(int?[,])", LanguageVersion::CSharp6), "(typeof int?[,])");
        assert_eq!(tree_at("typeof(int?[][])", LanguageVersion::CSharp6), "(typeof int?[][])");
        assert_eq!(tree_at("typeof(int? [ ])", LanguageVersion::CSharp6), "(typeof int?[])");
        assert_eq!(tree_at("a?[0]", LanguageVersion::CSharp6), "(?. a (index <recv> 0))");
        assert_eq!(tree_at("a?.B", LanguageVersion::CSharp6), "(?. a (. <recv> B))");
        assert_eq!(tree_at("a?.B.C", LanguageVersion::CSharp6), "(?. a (. (. <recv> B) C))");
        assert_eq!(
            tree_at("a?.B?.C", LanguageVersion::CSharp6),
            "(?. a (?. (. <recv> B) (. <recv> C)))"
        );
        assert_eq!(tree_at("a?.B++", LanguageVersion::CSharp6), "(post++ (?. a (. <recv> B)))");
        assert_eq!(codes_at("a?[0]", LanguageVersion::CSharp1), [8022]);
        assert_eq!(codes_at("a?.B", LanguageVersion::CSharp1), [8022]);
        assert_eq!(codes_at("typeof(int?[])", LanguageVersion::CSharp1), [8022]);
    }

    #[test]
    fn an_invalid_token_in_a_member_declaration_is_cs1519() {
        assert_eq!(unit_codes("class C { public static void }"), vec![1519]);
        assert_eq!(unit_codes("class C { int 5x; }"), vec![1519, 1519]);
        assert_eq!(unit_codes("class C { int 5x; int y; }"), vec![1519, 1519]);
    }

    #[test]
    fn a_duplicate_modifier_is_cs1004() {
        assert_eq!(unit_codes("class C { public public int x; }"), vec![1004]);
        assert_eq!(unit_codes("class C { static static int x; }"), vec![1004]);
        assert_eq!(unit_codes("public public class C {}"), vec![1004]);
        assert_eq!(unit_codes("class C { public static int x; }"), vec![]);
        assert_eq!(
            unit_codes("class C { public public public int x; }"),
            vec![1004, 1004]
        );
    }

    #[test]
    fn using_directives_and_namespaces() {
        assert_eq!(unit_tree("using System;"), "(using System)");
        assert_eq!(
            unit_tree("using System.Collections;"),
            "(using System.Collections)"
        );
        assert_eq!(
            unit_tree("using IO = System.IO;"),
            "(using-alias IO System.IO)"
        );
        assert_eq!(unit_tree("namespace N {}"), "(namespace N)");
        assert_eq!(
            unit_tree("namespace A.B { using System; }"),
            "(namespace A.B (using System))"
        );
    }

    #[test]
    fn classes_with_fields_and_methods() {
        assert_eq!(unit_tree("class C {}"), "(class C)");
        assert_eq!(
            unit_tree("public sealed class C : B, I {}"),
            "(class public sealed C : B I)"
        );
        assert_eq!(unit_tree("class C { int x; }"), "(class C (field int x))");
        assert_eq!(
            unit_tree("class C { public int x = 0, y; }"),
            "(class C (field public int x=0 y))"
        );
        assert_eq!(
            unit_tree("class C { void M() {} }"),
            "(class C (method void M () (block)))"
        );
        assert_eq!(
            unit_tree("class C { public static int Add(int a, int b) { return a + b; } }"),
            "(class C (method public static int Add (int a, int b) (block (return (+ a b)))))"
        );
        assert_eq!(
            unit_tree("interface I { void M(); }"),
            "(interface I (method void M () ;))"
        );
        assert_eq!(
            unit_tree("class C : I { int I.M() { return 1; } }"),
            "(class C : I (method int I.M () (block (return 1))))"
        );
        assert_eq!(
            unit_tree("class C { void M() { C c = new(); } }"),
            "(class C (method void M () (block (local C c=(new)))))"
        );
        assert_eq!(
            unit_tree("class C { void M() { C c = new(1, 2); } }"),
            "(class C (method void M () (block (local C c=(new 1 2)))))"
        );
        assert_eq!(
            unit_tree("class C { void M() { C c = new C(); } }"),
            "(class C (method void M () (block (local C c=(new C)))))"
        );
        assert_eq!(
            unit_tree("class C : N.I { int N.I.M() { return 1; } }"),
            "(class C : N.I (method int N.I.M () (block (return 1))))"
        );
        assert_eq!(
            unit_tree("class C : I { int I.this[int i] { get { return i; } } }"),
            "(class C : I (indexer I.this int [int i] (get (block (return i)))))"
        );
        assert_eq!(
            unit_tree("class C : N.I { int N.I.this[int i] { get { return i; } } }"),
            "(class C : N.I (indexer N.I.this int [int i] (get (block (return i)))))"
        );
        assert_eq!(
            unit_tree("class C : I<string> { string I<string>.this[int i] { get { return null; } } }"),
            "(class C : I<string> (indexer I<string>.this string [int i] (get (block (return null)))))"
        );
        assert_eq!(
            unit_tree("class C { int this[int i] { get { return i; } } }"),
            "(class C (indexer int [int i] (get (block (return i)))))"
        );
    }

    #[test]
    fn constructor_initializers() {
        assert_eq!(
            unit_tree("class C { C() : base() {} }"),
            "(class C (ctor C () :base() (block)))"
        );
        assert_eq!(
            unit_tree("class C { C(int x) : this(x, 0) {} }"),
            "(class C (ctor C (int x) :this(x 0) (block)))"
        );
        assert_eq!(
            unit_tree("class C { C() : base(1, 2) {} }"),
            "(class C (ctor C () :base(1 2) (block)))"
        );
    }

    #[test]
    fn constructors_and_parameter_modifiers() {
        assert_eq!(
            unit_tree("class C { C() {} }"),
            "(class C (ctor C () (block)))"
        );
        assert_eq!(
            unit_tree("class C { public C(int x) {} }"),
            "(class C (ctor public C (int x) (block)))"
        );
        assert_eq!(
            unit_tree("class C { void M(ref int a, out int b, params int[] xs) {} }"),
            "(class C (method void M (ref int a, out int b, params int[] xs) (block)))"
        );
    }

    #[test]
    fn a_whole_hello_world_program_parses() {
        let source = "using System; namespace Hello { class Program {   static void Main() { System.Console.WriteLine(\"Hi\"); } } }";
        assert_eq!(
            unit_tree(source),
            "(using System) (namespace Hello (class Program (method static void Main () (block (expr (call (. (. System Console) WriteLine) str))))))"
        );
    }

    #[test]
    fn declaration_diagnostics_match_the_reference_compiler() {
        assert_eq!(unit_codes("class C { int x }"), vec![1002]);
        assert_eq!(unit_codes("class C {"), vec![1513]);
    }

    /// A `@`-prefixed contextual keyword is an ordinary identifier (9.4.2), and every site that
    /// reads one has to ask the same question -- which is why they all ask
    /// [`Parser::current_contextual_keyword`] rather than comparing the text.
    ///
    /// **THE PAIRS ARE THE POINT.** Each plain spelling must keep working, or a guard that refuses
    /// everything would pass the verbatim half on its own. Both halves were scored against csc
    /// before this was written: `class Box<T> @where T : class` is CS1514 there and USED TO COMPILE
    /// here, and `@required int n;` is CS1519 there while this compiler read the modifier.
    #[test]
    fn a_verbatim_contextual_keyword_is_never_read_as_one() {
        assert_eq!(unit_codes_at("class Box<T> where T : class { }", LanguageVersion::CSharp2), vec![]);
        assert!(!unit_codes_at("class Box<T> @where T : class { }", LanguageVersion::CSharp2).is_empty());
        assert_eq!(unit_codes("class C { int P { get { return 1; } } }"), vec![]);
        assert!(!unit_codes("class C { int P { @get { return 1; } } }").is_empty());
        assert_eq!(
            unit_codes("class C { event System.EventHandler E { add { } remove { } } }"),
            vec![]
        );
        assert!(!unit_codes("class C { event System.EventHandler E { @add { } remove { } } }").is_empty());
    }

    #[test]
    fn parser_diagnostic_codes_are_confirmed_against_csc() {
        assert_eq!(unit_codes("using System"), vec![1002]);
        assert_eq!(unit_codes("class C { void M() { return 0 } }"), vec![1002]);
        assert_eq!(unit_codes("class C { void M() { try {} } }"), vec![1524]);
        assert_eq!(unit_codes("namespace { }"), vec![1001]);
        assert!(unit_codes("class C { void M() { foreach (int x xs) ; } }").contains(&1515));
        assert!(unit_codes("class C { void M() { if x) ; } }").contains(&1003));
        assert!(unit_codes("class C { void M() { f(1; } }").contains(&1026));
        assert!(unit_codes("class C { void M() { object o = typeof(); } }").contains(&1031));
    }

    /// `record` is a CONTEXTUAL keyword, and the disambiguation rule is measured rather than read
    /// off the grammar -- see [`Parser::record_declaration_here`] for the three csc rows.
    ///
    /// **THE FORMS ARE DISTINGUISHED IN THE DUMP BECAUSE THEY ARE DIFFERENT DECLARATIONS.**
    /// `record R` has no parameter list and `record R()` has an empty one; only the second gets a
    /// `Deconstruct`, so a tree that rendered them alike would make the difference untestable.
    #[test]
    fn record_declarations() {
        let at9 = |source| unit_tree_declaration(source, LanguageVersion::CSharp9);
        assert_eq!(at9("record R;"), "(record R : System.IEquatable<R>)");
        assert_eq!(at9("record R { }"), "(record R : System.IEquatable<R>)");
        assert_eq!(at9("record R();"), "(record R() : System.IEquatable<R> (ctor public R () (block)))");
        assert_eq!(
            at9("record R(int X);"),
            "(record R(int X) : System.IEquatable<R> (property public int X (get ;) (init ;)) (ctor public R (int X) (block (expr (= (. this X) X)))))"
        );
        assert_eq!(
            at9("public sealed record R(int X, string S);"),
            "(record public sealed R(int X, string S) : System.IEquatable<R> (property public int X (get ;) (init ;)) (property public string S (get ;) (init ;)) (ctor public R (int X, string S) (block (expr (= (. this X) X)) (expr (= (. this S) S)))))"
        );
        assert_eq!(
            at9("record R(int X) { }"),
            "(record R(int X) : System.IEquatable<R> (property public int X (get ;) (init ;)) (ctor public R (int X) (block (expr (= (. this X) X)))))"
        );
        assert_eq!(
            at9("record R(int X) { public int X; }"),
            "(record R(int X) : System.IEquatable<R> (field public int X) (ctor public R (int X) (block (expr (= (. this X) X)))))"
        );
        assert_eq!(
            at9("record D(int X, int Y) : B(X);"),
            "(record D(int X, int Y) : B(X) (property public int X (get ;) (init ;)) (property public int Y (get ;) (init ;)) (ctor public D (int X, int Y) (block (expr (= (. this X) X)) (expr (= (. this Y) Y)))))"
        );
        assert_eq!(
            at9("record D(int X) : B, I;"),
            "(record D(int X) : B I (property public int X (get ;) (init ;)) (ctor public D (int X) (block (expr (= (. this X) X)))))"
        );
        assert_eq!(
            at9("record record(int X);"),
            "(record record(int X) : System.IEquatable<record> (property public int X (get ;) (init ;)) (ctor public record (int X) (block (expr (= (. this X) X)))))"
        );
    }

    /// The four rows that say `record` is not read like `partial`, `async` or `required`: it takes
    /// the declaration whatever else is in scope, and only `@` gives the word back.
    #[test]
    fn record_is_contextual_but_not_speculative() {
        let at9 = |source| unit_tree_declaration(source, LanguageVersion::CSharp9);
        assert_eq!(at9("class C { record x; }"), "(class C (record x : System.IEquatable<x>))");
        assert_eq!(
            at9("class C { record R(int X); }"),
            "(class C (record R(int X) : System.IEquatable<R> (property public int X (get ;) (init ;)) (ctor public R (int X) (block (expr (= (. this X) X))))))"
        );
        assert_eq!(at9("class C { @record x; }"), "(class C (field record x))");
        assert_eq!(at9("class record { }"), "(class record)");
    }

    /// `record class` and `record struct` are a SEPARATE csc feature one rung higher, and csc
    /// calls both `'record structs'` -- plural, for the class form too. Deliberately unbuilt:
    /// over the pinned corpus the pair unlocks ONE file.
    #[test]
    fn the_record_keyword_forms_are_a_second_gate() {
        assert_eq!(
            unit_tree_declaration("record class R(int X);", LanguageVersion::CSharp10),
            "(record class R(int X) : System.IEquatable<R> (property public int X (get ;) (init ;)) (ctor public R (int X) (block (expr (= (. this X) X)))))"
        );
        assert_eq!(
            unit_tree_declaration("record struct R(int X);", LanguageVersion::CSharp10),
            "(record struct R(int X) : System.IEquatable<R> (property public int X (get ;) (init ;)) (ctor public R (int X) (block (expr (= (. this X) X)))))"
        );
        assert!(!unit_codes_at("record class R(int X);", LanguageVersion::CSharp9).is_empty());
        assert!(!unit_codes_at("record R(int X);", LanguageVersion::CSharp8).is_empty());
    }

    /// `init` occupies the SET slot and is spelled where `set` would be, so the tree keeps it as a
    /// flag on the setter rather than a third accessor -- csc answers `int P { init { } set { } }`
    /// with CS1007, which is what says the two are one slot.
    #[test]
    fn an_init_accessor_is_a_setter_and_is_invalid_on_a_static_member() {
        let at9 = |source| unit_codes_at(source, LanguageVersion::CSharp9);
        assert!(at9("class C { public static int P { get; init; } }").contains(&8856));
        assert!(!at9("class C { public int P { get; init; } }").contains(&8856));
        assert!(!at9("class C { public static int P { get; set; } }").contains(&8856));
    }

    #[test]
    fn enum_declarations() {
        assert_eq!(
            unit_tree("enum Color { Red, Green, Blue }"),
            "(enum Color Red Green Blue)"
        );
        assert_eq!(
            unit_tree("enum E : byte { A = 1, B = 2, }"),
            "(enum E : byte A=1 B=2)"
        );
        assert_eq!(unit_tree("public enum E {}"), "(enum public E)");
    }

    #[test]
    fn delegate_declarations() {
        assert_eq!(
            unit_tree("delegate void Handler(object sender, int code);"),
            "(delegate void Handler (object sender, int code))"
        );
        assert_eq!(
            unit_tree("public delegate int F();"),
            "(delegate public int F ())"
        );
    }

    /// `delegate void D<T>(T value);` (C# 2.0; ECMA-334 4th ed 22.1). It did not parse at all --
    /// `CS1003`, and on 11 programs of the cstest campaign -- because [`Parser::parse_delegate`]
    /// went straight from the name to the parameter list.
    #[test]
    fn generic_delegate_declarations() {
        assert_eq!(
            unit_tree("delegate void D<T>(T value);"),
            "(delegate void D<T> (T value))"
        );
        assert_eq!(
            unit_tree("public delegate R Project<T, R>(T value);"),
            "(delegate public R Project<T, R> (T value))"
        );
        assert_eq!(
            unit_tree("delegate R P<T, R>(T value) where R : class;"),
            "(delegate R P<T, R> (T value) where R : class)"
        );
        assert_eq!(
            unit_tree("delegate T N<T>() where T : struct;"),
            "(delegate T N<T> () where T : struct)"
        );
        assert_eq!(
            unit_tree("delegate T M<T>() where T : System.IDisposable, new();"),
            "(delegate T M<T> () where T : System.IDisposable, new())"
        );
        assert_eq!(unit_tree("delegate void D(int v);"), "(delegate void D (int v))");
    }

    /// `using T x = e;` (C# 8.0) is the `using` STATEMENT with its body implied, and the tree it
    /// produces is the tree the statement spelling produces -- which is the whole feature.
    ///
    /// The body is THE REST OF THE ENCLOSING BLOCK, so these assertions are about where the
    /// following statements land, not about the declaration itself.
    #[test]
    fn using_declarations() {
        assert_eq!(
            unit_tree("class C { void M() { using D d = e; f(); } }"),
            "(class C (method void M () (block (using (local D d=e) (block (expr (call f)))))))"
        );
        assert_eq!(
            unit_tree("class C { void M() { using D d = e; } }"),
            "(class C (method void M () (block (using (local D d=e) (block)))))"
        );
        assert_eq!(
            unit_tree("class C { void M() { using D a = e; using D b = e; f(); } }"),
            "(class C (method void M () (block (using (local D a=e) (block (using (local D b=e) \
             (block (expr (call f)))))))))"
                .replace("             ", "")
                .as_str()
        );
        assert_eq!(
            unit_tree("class C { void M() { g(); using D d = e; f(); } }"),
            "(class C (method void M () (block (expr (call g)) (using (local D d=e) \
             (block (expr (call f)))))))"
                .replace("             ", "")
                .as_str()
        );
        assert_eq!(
            unit_tree("class C { void M() { { using D d = e; f(); } h(); } }"),
            "(class C (method void M () (block (block (using (local D d=e) \
             (block (expr (call f))))) (expr (call h)))))"
                .replace("             ", "")
                .as_str()
        );
        assert_eq!(
            unit_tree("class C { void M() { using (D d = e) { f(); } } }"),
            "(class C (method void M () (block (using (local D d=e) (block (expr (call f)))))))"
        );
        assert_eq!(
            unit_codes("class C { void M(bool c) { if (c) using D d = e; f(); } }"),
            vec![1023]
        );
        assert_eq!(
            unit_tree_ignoring_gates(
                "class C { void M(bool c) { if (c) using D d = e; f(); } }",
                LanguageVersion::DEFAULT,
            ),
            "(class C (method void M (bool c) (block (if c (using (local D d=e) (block))) \
             (expr (call f)))))"
                .replace("             ", "")
                .as_str()
        );
    }

    /// A type or namespace declaration may be followed by ONE optional semicolon -- ECMA-334 spells
    /// `class-body ;opt` and the same for a struct, an interface, an enum and a namespace body.
    ///
    /// It was accepted after a RECORD alone (`CS1518` on the other five), which is the C-and-C++
    /// habit refused in exactly the source that carries it over.
    #[test]
    fn optional_semicolon_after_a_declaration() {
        assert_eq!(unit_tree("class C { };"), "(class C)");
        assert_eq!(unit_tree("struct S { };"), "(struct S)");
        assert_eq!(unit_tree("interface I { };"), "(interface I)");
        assert_eq!(unit_tree("enum E { A };"), "(enum E A)");
        assert_eq!(unit_tree("namespace N { class C {} };"), "(namespace N (class C))");
        assert_eq!(
            unit_tree("class O { class A { }; class B { } }"),
            "(class O (class A) (class B))"
        );
        assert_eq!(unit_codes("class C { };"), Vec::<u16>::new());
        assert!(!unit_codes("class C { };;").is_empty());
    }

    #[test]
    fn properties() {
        assert_eq!(
            unit_tree("class C { int X { get; set; } }"),
            "(class C (property int X (get ;) (set ;)))"
        );
        assert_eq!(
            unit_tree("class C { int X { get { return x; } } }"),
            "(class C (property int X (get (block (return x)))))"
        );
        assert_eq!(
            unit_tree("class C { public int P { get { return 1; } set {} } }"),
            "(class C (property public int P (get (block (return 1))) (set (block))))"
        );
    }

    #[test]
    fn attributes() {
        assert_eq!(
            unit_tree("[Serializable] class C {}"),
            "[Serializable] (class C)"
        );
        assert_eq!(
            unit_tree("[Obsolete(\"x\")] public class C {}"),
            "[Obsolete(str)] (class public C)"
        );
        assert_eq!(unit_tree("[A, B] enum E { X }"), "[A, B] (enum E X)");
        assert_eq!(
            unit_tree("[Conditional(\"DEBUG\")] delegate void D();"),
            "[Conditional(str)] (delegate void D ())"
        );
        assert_eq!(
            unit_tree("class C { [Obsolete] void M([In] int x) {} }"),
            "(class C (method void M (int x) (block)))"
        );
    }

    #[test]
    fn destructors() {
        assert_eq!(
            unit_tree("class C { ~C() {} }"),
            "(class C (dtor C (block)))"
        );
        assert_eq!(
            unit_tree("class C { ~C() { Cleanup(); } }"),
            "(class C (dtor C (block (expr (call Cleanup)))))"
        );
    }

    #[test]
    fn operators() {
        assert_eq!(
            unit_tree("class C { public static C operator +(C a, C b) { return a; } }"),
            "(class C (operator public static C + (C a, C b) (block (return a))))"
        );
        assert_eq!(
            unit_tree("class C { public static bool operator ==(C a, C b) { return true; } }"),
            "(class C (operator public static bool == (C a, C b) (block (return true))))"
        );
        assert_eq!(
            unit_tree("class C { public static implicit operator int(C c) { return 0; } }"),
            "(class C (conversion public static implicit int (C c) (block (return 0))))"
        );
        assert_eq!(
            unit_tree("class C { public static explicit operator C(int n) { return null; } }"),
            "(class C (conversion public static explicit C (int n) (block (return null))))"
        );
    }

    #[test]
    fn events() {
        assert_eq!(
            unit_tree("class C { event Handler Click; }"),
            "(class C (event-field Handler Click))"
        );
        assert_eq!(
            unit_tree("class C { public event Handler A, B; }"),
            "(class C (event-field public Handler A B))"
        );
        assert_eq!(
            unit_tree("class C { event Handler E { add {} remove {} } }"),
            "(class C (event Handler E (add (block)) (remove (block))))"
        );
    }

    #[test]
    fn indexers() {
        assert_eq!(
            unit_tree("class C { int this[int i] { get; set; } }"),
            "(class C (indexer int [int i] (get ;) (set ;)))"
        );
        assert_eq!(
            unit_tree("class C { string this[int x, int y] { get { return s; } } }"),
            "(class C (indexer string [int x, int y] (get (block (return s)))))"
        );
    }

    /// A small alphabet of token spellings the fuzzer draws from.
    const FUZZ_TOKENS: &[&str] = &[
        "class",
        "struct",
        "interface",
        "enum",
        "delegate",
        "namespace",
        "using",
        "public",
        "static",
        "void",
        "int",
        "string",
        "bool",
        "object",
        "new",
        "return",
        "if",
        "else",
        "while",
        "for",
        "foreach",
        "do",
        "switch",
        "case",
        "default",
        "break",
        "continue",
        "throw",
        "try",
        "catch",
        "finally",
        "lock",
        "checked",
        "unchecked",
        "this",
        "base",
        "operator",
        "implicit",
        "explicit",
        "get",
        "set",
        "add",
        "remove",
        "event",
        "const",
        "ref",
        "out",
        "params",
        "goto",
        "is",
        "as",
        "typeof",
        "null",
        "true",
        "false",
        "{",
        "}",
        "(",
        ")",
        "[",
        "]",
        ";",
        ",",
        ".",
        ":",
        "=",
        "==",
        "+",
        "-",
        "*",
        "/",
        "%",
        "&",
        "|",
        "^",
        "!",
        "~",
        "<",
        ">",
        "<=",
        ">=",
        "<<",
        ">>",
        "?",
        "++",
        "--",
        "@",
        "x",
        "Foo",
        "M",
        "a",
        "0",
        "42",
        "\"s\"",
    ];

    fn fuzz_step(state: u64) -> u64 {
        state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407)
    }

    #[test]
    fn parsing_arbitrary_token_soup_never_panics() {
        let mut state: u64 = 0x1234_5678_9abc_def0;
        for seed in 0u64..4000 {
            state = fuzz_step(state.wrapping_add(seed));
            let mut walk = state;
            let token_count = (walk % 40) as usize + 1;
            let mut input = String::new();
            for _ in 0..token_count {
                walk = fuzz_step(walk);
                input.push_str(FUZZ_TOKENS[(walk >> 33) as usize % FUZZ_TOKENS.len()]);
                input.push(' ');
            }
            let _ = parse_compilation_unit(&input);
            let _ = parse_statement(&input);
            let _ = parse_expression(&input);
        }
    }

    #[test]
    fn parsing_every_prefix_of_a_program_never_panics() {
        let corpus = [
            "using System; namespace N { class C : B { public int F = 0; void M(ref int a) { for (int i = 0; i < 10; i++) { f(i); } } C() : base() {} int P { get; set; } int this[int i] { get { return 0; } } } }",
            "[Serializable] enum E : byte { A, B = 2, } delegate int D(string s);",
            "class C { public static C operator +(C a, C b) { return a; } ~C() {} event H E { add {} remove {} } int[] xs = { 1, 2, 3 }; }",
        ];
        for source in corpus {
            for end in 0..=source.len() {
                if source.is_char_boundary(end) {
                    let _ = parse_compilation_unit(&source[..end]);
                }
            }
        }
    }

    #[test]
    fn nested_types() {
        assert_eq!(
            unit_tree("class Outer { class Inner {} }"),
            "(class Outer (class Inner))"
        );
        assert_eq!(
            unit_tree("class C { enum E { A } }"),
            "(class C (enum E A))"
        );
        assert_eq!(
            unit_tree("namespace N { class C { delegate void D(); } }"),
            "(namespace N (class C (delegate void D ())))"
        );
    }

    /// A deeply nested expression recurses the parser as deep as the input nests. The compiler
    /// drivers (lcsc and the compile-file harness) compile on a large-stack worker thread so this
    /// cannot overflow the small default main-thread stack. Parsing the same shape here on a
    /// thread with a generous stack -- the default test-thread stack (about 2 MiB) would itself
    /// overflow -- confirms the parser carries no pathological per-frame cost. The worker matches
    /// the drivers' 64 MiB (which follows a full debug compile past depth 3750; a parse alone costs
    /// less per level, so depth 3000 clears with wide margin). Both the parse and the deep tree's
    /// recursive drop run on the worker, so only the diagnostic count crosses back.
    #[test]
    fn a_deeply_nested_expression_parses_on_a_generous_stack() {
        let depth = 3000;
        let mut source = String::from("class C { void M() { int x = ");
        source.push_str(&"(".repeat(depth));
        source.push('1');
        source.push_str(&")".repeat(depth));
        source.push_str("; } }");

        let diagnostic_count = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || parse_compilation_unit(&source).diagnostics.len())
            .expect("spawn a generous-stack parse thread")
            .join()
            .expect("the deep-nesting parse thread panicked");

        assert_eq!(
            diagnostic_count, 0,
            "a balanced deeply nested expression parses without a syntax error"
        );
    }
}
