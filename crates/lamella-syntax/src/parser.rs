//! The parser: building a syntax tree from the token stream.

use crate::ast::{
    Accessor, AssignmentOperator, Attribute, AttributeArgument, AttributeSection, BinaryOperator,
    CatchClause, CompilationUnit, ConstructorInitializer, ConstructorInitializerKind,
    ConversionDirection, DelegateDecl, EnumDecl, EnumMember, Expr, ExprKind, ForInitializer,
    GotoTarget, Initializer, Literal, Member, MemberInitializer, MemberInitializerValue, Modifier,
    NamespaceDecl, NamespaceMember, OverloadableOperator,
    Parameter, ParameterModifier, PostfixOperator, PredefinedType, QualifiedName, Stmt, StmtKind,
    SwitchLabel, SwitchSection, TypeDecl, TypeKind, TypeParameter, TypeParameterConstraint,
    TypeParameterConstraintClause, TypeRef, TypeRefKind,
    TypeTestOperation,
    UnaryOperator, UsingDirective, UsingKind, UsingResource, VariableDeclarator,
};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::lexer::{LexOptions, Tokenized, tokenize, tokenize_with};
use crate::span::Span;
use crate::version::{Feature, LanguageVersion};
use crate::token::{Keyword, Punctuator, Token, TokenKind, TypedRefKeyword};
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
    let mut parser = Parser::new(tokenize_with(source, options));
    parser.version = version;
    let unit = parser.parse_compilation_unit();
    ParsedCompilationUnit {
        unit,
        diagnostics: without_gated_operator_cascades(parser.diagnostics),
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

    /// The current token's identifier text, if it is an identifier. Used for the
    /// contextual `get`/`set` accessor names, which are not keywords.
    fn current_identifier_text(&self) -> Option<&str> {
        match &self.current().kind {
            TokenKind::Identifier(text) => Some(text),
            _ => None,
        }
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
                    return self.parse_block();
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
        let mut statements = Vec::new();
        while self.current_punctuator() != Some(Punctuator::CloseBrace)
            && !matches!(self.current().kind, TokenKind::EndOfFile)
        {
            let before = self.position;
            statements.push(self.parse_statement());
            if self.position == before {
                self.bump();
            }
        }
        let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
        Stmt::new(StmtKind::Block(statements), Span::new(start, end))
    }

    /// Parses a `return` statement (15.9.4): `return expression_opt ;`.
    fn parse_return(&mut self, start: u32) -> Stmt {
        self.bump();
        let value = if self.current_punctuator() == Some(Punctuator::Semicolon) {
            None
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

    /// Parses one or more comma-separated variable declarators (15.5.1), each an
    /// identifier with an optional `= expression` initializer. Array initializers
    /// are not yet parsed. Does not consume a terminator.
    fn parse_variable_declarators(&mut self) -> Vec<VariableDeclarator> {
        let mut declarators = Vec::new();
        loop {
            let declarator_start = self.current().span.start;
            let (name, mut end) = self.expect_identifier();
            let initializer = if self.eat(Punctuator::Equals) {
                let value = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                    self.parse_array_initializer()
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
        let declarators = self.parse_variable_declarators();
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
            let declarators = self.parse_variable_declarators();
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
        let ty = self.parse_type();
        let (name, _) = self.expect_identifier();
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

    /// Parses one `catch` clause (15.10): an optional `( type name_opt )` then a
    /// block. A bare `catch` is a general catch.
    fn parse_catch_clause(&mut self) -> CatchClause {
        let start = self.current().span.start;
        let mut end = self.current().span.end;
        self.bump();
        let (exception_type, name) = if self.eat(Punctuator::OpenParen) {
            let ty = self.parse_type();
            let name = if matches!(self.current().kind, TokenKind::Identifier(_)) {
                Some(self.expect_identifier().0)
            } else {
                None
            };
            end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
            (Some(ty), name)
        } else {
            (None, None)
        };
        let body = Box::new(self.parse_required_block());
        CatchClause {
            exception_type,
            name,
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
            let declarators = self.parse_variable_declarators();
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

    /// Parses one `using` directive (16.3): a namespace import or an alias.
    fn parse_using_directive(&mut self) -> UsingDirective {
        let start = self.current().span.start;
        self.bump();
        if self.current_keyword() == Some(Keyword::Static) {
            self.report(
                DiagnosticKind::FeatureRequiresLaterVersion {
                    feature: crate::version::Feature::UsingStatic.description(),
                    required: crate::version::Feature::UsingStatic
                        .introduced_in()
                        .required_name(),
                            current: self.version,
                },
                Span::empty_at(self.current().span.start),
            );
            self.bump();
        }
        let kind = if matches!(self.current().kind, TokenKind::Identifier(_))
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
        EnumDecl {
            attributes,
            modifiers,
            name,
            base,
            members,
            span: Span::new(start, end),
        }
    }

    /// Parses a `delegate` declaration (22): `delegate return-type name ( params ) ;`.
    fn parse_delegate(
        &mut self,
        attributes: Vec<AttributeSection>,
        modifiers: Vec<Modifier>,
        start: u32,
    ) -> DelegateDecl {
        self.bump();
        let return_type = self.parse_type();
        let (name, _) = self.expect_identifier();
        let (parameters, arglist) = self.parse_parameter_list();
        if let Some(span) = arglist {
            self.report(DiagnosticKind::ArglistNotValidInThisContext, span);
        }
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        DelegateDecl {
            attributes,
            modifiers,
            return_type,
            name,
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
        if named && !self.version.supports(Feature::FileScopedNamespaces) {
            self.report(
                DiagnosticKind::FeatureRequiresLaterVersion {
                    feature: Feature::FileScopedNamespaces.description(),
                    required: Feature::FileScopedNamespaces.introduced_in().required_name(),
                    current: self.version,
                },
                keyword,
            );
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
        let kind = match self.current_keyword() {
            Some(Keyword::Class) => {
                self.bump();
                TypeKind::Class
            }
            Some(Keyword::Struct) => {
                self.bump();
                TypeKind::Struct
            }
            Some(Keyword::Interface) => {
                self.bump();
                TypeKind::Interface
            }
            _ => {
                let at = self.current().span.start;
                self.report(DiagnosticKind::TypeDeclarationExpected, Span::empty_at(at));
                TypeKind::Class
            }
        };
        let (name, _) = self.expect_identifier();
        let type_parameters = self.parse_type_parameter_list();
        let bases = if self.eat(Punctuator::Colon) {
            let mut bases = Vec::new();
            bases.push(self.parse_type());
            while self.eat(Punctuator::Comma) {
                bases.push(self.parse_type());
            }
            bases
        } else {
            Vec::new()
        };
        let constraints = self.parse_type_parameter_constraint_clauses();
        self.expect(Punctuator::OpenBrace, DiagnosticKind::OpenBraceExpected);
        let mut members = Vec::new();
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
        TypeDecl {
            attributes,
            modifiers,
            kind,
            name,
            type_parameters,
            bases,
            constraints,
            members,
            span: Span::new(start, end),
        }
    }

    /// Parses a run of leading declaration modifiers (17.2 and elsewhere). The
    /// parser accepts any; binding checks which are valid where.
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
        if !matches!(&self.current().kind, TokenKind::Identifier(text) if &**text == "required") {
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

    fn parse_modifiers(&mut self) -> Vec<Modifier> {
        let mut modifiers = Vec::new();
        loop {
            let modifier = match self.current_keyword().and_then(modifier_of) {
                Some(modifier) => modifier,
                None if self.required_is_a_modifier_here() => {
                    if !self.version.supports(Feature::RequiredMembers) {
                        self.report(
                            DiagnosticKind::FeatureRequiresLaterVersion {
                                feature: Feature::RequiredMembers.description(),
                                required: Feature::RequiredMembers
                                    .introduced_in()
                                    .required_name(),
                                current: self.version,
                            },
                            self.current().span,
                        );
                    }
                    Modifier::Required
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
        if matches!(
            self.current_keyword(),
            Some(Keyword::Class)
                | Some(Keyword::Struct)
                | Some(Keyword::Interface)
                | Some(Keyword::Enum)
                | Some(Keyword::Delegate)
        ) {
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
            let body = self.parse_required_block();
            let end = body.span.end;
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
        let ty = self.parse_type();
        if self.current_keyword() == Some(Keyword::Operator) {
            return self.parse_operator(modifiers, ty, start);
        }
        if self.current_keyword() == Some(Keyword::This) {
            return self.parse_indexer(modifiers, ty, start);
        }
        if matches!(self.current().kind, TokenKind::Identifier(_))
            && (self.next_is(Punctuator::OpenParen)
                || (self.next_is(Punctuator::LessThan)
                    && !self.explicit_interface_qualifier_ahead()))
        {
            let (name, _) = self.expect_identifier();
            let type_parameters = self.parse_type_parameter_list();
            let (parameters, arglist) = self.parse_parameter_list();
            let constraints = self.parse_type_parameter_constraint_clauses();
            let (body, end) = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                let block = self.parse_block();
                let end = block.span.end;
                (Some(block), end)
            } else {
                let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
                (None, end)
            };
            return Member::Method {
                modifiers,
                return_type: ty,
                name,
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
            let restore_position = self.position;
            let restore_diagnostics = self.diagnostics.len();
            let name_start = self.current().span.start;
            let (first, mut prev_end) = self.expect_identifier();
            let mut parts = Vec::new();
            parts.push(first);
            let mut arguments: Vec<TypeRef> = Vec::new();
            let mut arguments_on: Option<usize> = None;
            let mut interface_end = prev_end;
            if self.current_punctuator() == Some(Punctuator::LessThan)
                && self.generic_type_name_ahead()
            {
                let (list, list_end, _) = self.parse_type_argument_list(false);
                arguments = list;
                arguments_on = Some(parts.len() - 1);
                prev_end = list_end;
            }
            while self.current_punctuator() == Some(Punctuator::Dot) {
                self.bump();
                interface_end = prev_end;
                let (part, part_end) = self.expect_identifier();
                parts.push(part);
                prev_end = part_end;
                if self.current_punctuator() == Some(Punctuator::LessThan)
                    && self.generic_type_name_ahead()
                {
                    let (list, list_end, _) = self.parse_type_argument_list(false);
                    arguments = list;
                    arguments_on = Some(parts.len() - 1);
                    prev_end = list_end;
                }
            }
            let member = parts.pop().expect("a qualified member name has >= 2 parts");
            if arguments.is_empty() || arguments_on == Some(parts.len().saturating_sub(1)) {
                let interface_kind = if arguments.is_empty() {
                    TypeRefKind::Name(parts)
                } else {
                    TypeRefKind::Generic { parts, arguments }
                };
                let explicit_interface =
                    TypeRef::new(interface_kind, Span::new(name_start, interface_end));
                if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                    return self
                        .parse_property(modifiers, ty, member, Some(explicit_interface), start);
                }
                let type_parameters = self.parse_type_parameter_list();
                let (parameters, arglist) = self.parse_parameter_list();
                let constraints = self.parse_type_parameter_constraint_clauses();
                let (body, end) = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                    let block = self.parse_block();
                    let end = block.span.end;
                    (Some(block), end)
                } else {
                    let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
                    (None, end)
                };
                return Member::Method {
                    modifiers,
                    return_type: ty,
                    name: member,
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
            self.position = restore_position;
            self.diagnostics.truncate(restore_diagnostics);
        }
        if matches!(self.current().kind, TokenKind::Identifier(_))
            && self.next_is(Punctuator::OpenBrace)
        {
            let (name, _) = self.expect_identifier();
            return self.parse_property(modifiers, ty, name, None, start);
        }
        if matches!(self.current().kind, TokenKind::Identifier(_)) {
            let declarators = self.parse_variable_declarators();
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
            if matches!(
                self.current_keyword(),
                Some(Keyword::Public | Keyword::Protected | Keyword::Internal | Keyword::Private)
            ) {
                let at = self.current().span.start;
                self.report(
                    DiagnosticKind::FeatureRequiresLaterVersion {
                        feature: crate::version::Feature::AccessorAccessibility.description(),
                        required: crate::version::Feature::AccessorAccessibility
                            .introduced_in()
                            .required_name(),
                                current: self.version,
                    },
                    Span::empty_at(at),
                );
                while matches!(
                    self.current_keyword(),
                    Some(Keyword::Public | Keyword::Protected | Keyword::Internal | Keyword::Private)
                ) {
                    self.bump();
                }
            }
            let is_getter = match self.current_identifier_text() {
                Some("get") => true,
                Some("set") => false,
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
                body,
                span: Span::new(accessor_start, accessor_end),
            };
            if is_getter {
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
        explicit_interface: Option<TypeRef>,
        start: u32,
    ) -> Member {
        let (getter, setter, end) = self.parse_accessor_block();
        Member::Property {
            modifiers,
            ty,
            name,
            getter,
            setter,
            explicit_interface,
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
        let declarators = self.parse_variable_declarators();
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
            let is_adder = match self.current_identifier_text() {
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
                body,
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
        let body = self.parse_required_block();
        let end = body.span.end;
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
        let body = self.parse_required_block();
        let end = body.span.end;
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
        let body = self.parse_required_block();
        let end = body.span.end;
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
    fn parse_indexer(&mut self, modifiers: Vec<Modifier>, ty: TypeRef, start: u32) -> Member {
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
        let (getter, setter, end) = self.parse_accessor_block();
        Member::Indexer {
            modifiers,
            ty,
            parameters,
            getter,
            setter,
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
            let _ = self.parse_attribute_sections();
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
            let (name, mut end) = self.expect_identifier();
            if self.current_punctuator() == Some(Punctuator::Equals) {
                let at = self.current().span.start;
                self.report(
                    DiagnosticKind::FeatureRequiresLaterVersion {
                        feature: crate::version::Feature::DefaultParameterValues.description(),
                        required: crate::version::Feature::DefaultParameterValues
                            .introduced_in()
                            .required_name(),
                                current: self.version,
                    },
                    Span::empty_at(at),
                );
                self.bump();
                end = self.parse_expression().span.end;
            }
            parameters.push(Parameter {
                modifier,
                ty,
                name,
                span: Span::new(start, end),
            });
            if !self.eat(Punctuator::Comma) {
                break;
            }
        }
        (parameters, arglist)
    }

    /// Parses a full expression (14): an assignment, which sits at the bottom of
    /// the precedence ladder.
    fn parse_expression(&mut self) -> Expr {
        self.parse_assignment()
    }

    /// Assignment (14.14), right-associative and lower than the conditional. The
    /// target is parsed as a conditional and validated as an lvalue when binding,
    /// matching how csc parses then checks.
    fn parse_assignment(&mut self) -> Expr {
        let target = self.parse_conditional();
        let Some(operator) = self.current_punctuator().and_then(assignment_operator) else {
            return target;
        };
        self.bump();
        let value = self.parse_assignment();
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
        let condition = self.parse_binary(1);
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

    /// The binary operators (14.7 through 14.12) by precedence climbing.
    /// `minimum` is the lowest precedence this call will accept; all the
    /// operators are left-associative.
    fn parse_binary(&mut self, minimum: u8) -> Expr {
        const RELATIONAL: u8 = 7;
        let mut left = self.parse_unary();
        loop {
            if RELATIONAL >= minimum {
                if let Some(operation) = type_test_operation(&self.current().kind) {
                    self.bump();
                    let target = self.parse_type_inner(false);
                    let span = Span::new(left.span.start, target.span.end);
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

    /// Unary expressions (14.6): a prefix operator, a cast, or a postfix chain.
    fn parse_unary(&mut self) -> Expr {
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
        let mut expr = self.parse_primary();
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
                    let (arguments, end) = self.parse_arguments(
                        Punctuator::CloseBracket,
                        DiagnosticKind::TokenExpected { expected: "]" },
                    );
                    let span = Span::new(expr.span.start, end);
                    expr = Expr::new(
                        ExprKind::ElementAccess {
                            receiver: Box::new(expr),
                            arguments,
                        },
                        span,
                    );
                }
                Some(Punctuator::PlusPlus) => {
                    expr = self.finish_postfix(expr, PostfixOperator::Increment);
                }
                Some(Punctuator::MinusMinus) => {
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
                self.bump();
                Expr::new(ExprKind::Name(name), span)
            }
            TokenKind::Punctuator(Punctuator::OpenParen) => {
                self.bump();
                let inner = self.parse_expression();
                let end = self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
                Expr::new(
                    ExprKind::Parenthesized(Box::new(inner)),
                    Span::new(span.start, end),
                )
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
                self.report(
                    DiagnosticKind::FeatureRequiresLaterVersion {
                        feature: crate::version::Feature::AnonymousMethods.description(),
                        required: crate::version::Feature::AnonymousMethods
                            .introduced_in()
                            .required_name(),
                                current: self.version,
                    },
                    Span::empty_at(span.start),
                );
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

    /// Parses a `,`-separated argument list up to and including `close` (14.4.1),
    /// returning the arguments and the offset just past the closing bracket.
    fn parse_arguments(&mut self, close: Punctuator, missing: DiagnosticKind) -> (Vec<Expr>, u32) {
        let mut arguments = Vec::new();
        if self.current_punctuator() == Some(close) {
            let end = self.current().span.end;
            self.bump();
            return (arguments, end);
        }
        loop {
            let before = self.position;
            if matches!(self.current().kind, TokenKind::Identifier(_))
                && self.next_is(Punctuator::Colon)
            {
                let at = self.current().span.start;
                self.report(
                    DiagnosticKind::FeatureRequiresLaterVersion {
                        feature: crate::version::Feature::NamedArguments.description(),
                        required: crate::version::Feature::NamedArguments
                            .introduced_in()
                            .required_name(),
                                current: self.version,
                    },
                    Span::empty_at(at),
                );
                self.bump();
                self.bump();
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
            let argument = self.parse_expression();
            let argument = match ref_out {
                Some(out) => {
                    let span = argument.span;
                    Expr::new(
                        ExprKind::RefArgument {
                            out,
                            operand: Box::new(argument),
                        },
                        span,
                    )
                }
                None => argument,
            };
            arguments.push(argument);
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
    fn parse_non_array_type(&mut self) -> TypeRef {
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
        if allow_nullable
            && self.current_punctuator() == Some(Punctuator::Question)
            && matches!(&base.kind, TypeRefKind::Predefined(p) if is_predefined_value_type(*p))
        {
            let at = self.current().span.start;
            self.report(
                DiagnosticKind::FeatureRequiresLaterVersion {
                    feature: crate::version::Feature::NullableValueTypes.description(),
                    required: crate::version::Feature::NullableValueTypes
                        .introduced_in()
                        .required_name(),
                            current: self.version,
                },
                Span::empty_at(at),
            );
            self.bump();
        }
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
            self.report(
                DiagnosticKind::FeatureRequiresLaterVersion {
                    feature: crate::version::Feature::AnonymousObjectCreation.description(),
                    required: crate::version::Feature::AnonymousObjectCreation
                        .introduced_in()
                        .required_name(),
                            current: self.version,
                },
                Span::empty_at(start),
            );
            let end = self.skip_balanced(Punctuator::OpenBrace, Punctuator::CloseBrace);
            return Expr::new(ExprKind::Error, Span::new(start, end));
        }
        let element = self.parse_non_array_type();
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
                        target: element,
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
                        target: element,
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
                        target: element,
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
    /// **Only that half of the two-bit rule lives here.** The other half -- a dialect that PERMITS
    /// an initializer while this build cannot produce one -- is the binder's, because only it has
    /// `LAM0001`. The two conditions are disjoint, so a program never draws both.
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
        if self.version.supports(feature) {
            return;
        }
        self.report(
            DiagnosticKind::FeatureRequiresLaterVersion {
                feature: feature.description(),
                required: feature.introduced_in().required_name(),
                current: self.version,
            },
            Span::empty_at(self.current().span.start),
        );
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
                elements.push(self.parse_expression());
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
    fn parse_member_initializer(&mut self) -> MemberInitializer {
        let start = self.current().span.start;
        let (name, mut end) = self.expect_identifier();
        self.expect(Punctuator::Equals, DiagnosticKind::TokenExpected { expected: "=" });
        let value = if self.current_punctuator() == Some(Punctuator::OpenBrace) {
            let (nested, nested_end) = self.parse_initializer();
            end = nested_end;
            MemberInitializerValue::Nested(nested)
        } else {
            let expression = self.parse_expression();
            end = expression.span.end;
            MemberInitializerValue::Expression(expression)
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
            let (sizes, end) = self.parse_arguments(
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
    /// A specifier on a NON-FINAL part -- `typeof(Outer<>.Inner)` -- is not accepted, though the
    /// grammar allows one after every part. [`TypeRefKind::Unbound`] carries ONE arity for a whole
    /// dotted name because [`TypeRefKind::Generic`] carries ONE argument list for one, so the
    /// constructed form `List<int>.Enumerator` and the unbound form `List<>.Enumerator` are
    /// unrepresentable for the same reason. This answers `None` for it and the ordinary type parse
    /// refuses, which is what it already does for the constructed form.
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
    fn parse_type_name_inner(&mut self, nested: bool) -> (TypeRef, AngleCredit) {
        let start = self.current().span.start;
        let (first, mut end) = self.expect_identifier();
        let mut parts = Vec::new();
        parts.push(first);
        while self.current_punctuator() == Some(Punctuator::Dot) {
            self.bump();
            let (part, part_end) = self.expect_identifier();
            parts.push(part);
            end = part_end;
        }
        if self.current_punctuator() != Some(Punctuator::LessThan) {
            return (
                TypeRef::new(TypeRefKind::Name(parts), Span::new(start, end)),
                AngleCredit::default(),
            );
        }
        if !self.version.supports(Feature::Generics) {
            let at = self.current().span.start;
            self.report(
                DiagnosticKind::FeatureRequiresLaterVersion {
                    feature: crate::version::Feature::Generics.description(),
                    required: crate::version::Feature::Generics
                        .introduced_in()
                        .required_name(),
                    current: self.version,
                },
                Span::empty_at(at),
            );
            end = self.skip_type_argument_list();
            return (
                TypeRef::new(TypeRefKind::Name(parts), Span::new(start, end)),
                AngleCredit::default(),
            );
        }
        let (arguments, list_end, credit) = self.parse_type_argument_list(nested);
        (
            TypeRef::new(
                TypeRefKind::Generic { parts, arguments },
                Span::new(start, list_end),
            ),
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
            self.report(
                DiagnosticKind::FeatureRequiresLaterVersion {
                    feature: crate::version::Feature::Generics.description(),
                    required: crate::version::Feature::Generics
                        .introduced_in()
                        .required_name(),
                    current: self.version,
                },
                Span::empty_at(at),
            );
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
        if !matches!(&self.current().kind, TokenKind::Identifier(name) if &**name == "where") {
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::String;

    /// Renders an expression as a parenthesized prefix form, so a test can assert
    /// on structure (and thus precedence and associativity) in one readable line.
    fn dump(expr: &Expr) -> String {
        match &expr.kind {
            ExprKind::Literal(Literal::Integer { value, .. }) => format!("{value}"),
            ExprKind::Literal(Literal::Real { .. }) => String::from("real"),
            ExprKind::Literal(Literal::Decimal { .. }) => String::from("decimal"),
            ExprKind::Literal(Literal::Character(unit)) => format!("char:{unit}"),
            ExprKind::Literal(Literal::String(_)) => String::from("str"),
            ExprKind::Literal(Literal::Boolean(value)) => format!("{value}"),
            ExprKind::Literal(Literal::Null) => String::from("null"),
            ExprKind::Name(name) => String::from(&**name),
            ExprKind::PredefinedType(predefined) => String::from(predefined_text(*predefined)),
            ExprKind::This => String::from("this"),
            ExprKind::Base => String::from("base"),
            ExprKind::Parenthesized(inner) => format!("(paren {})", dump(inner)),
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
            ExprKind::RefArgument { out, operand } => {
                format!("({} {})", if *out { "out" } else { "ref" }, dump(operand))
            }
            ExprKind::Unary { operator, operand } => {
                format!("({} {})", unary_text(*operator), dump(operand))
            }
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
                    text.push_str(&dump(argument));
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
                format!("({text} {} {})", dump(operand), dump_type(target))
            }
            ExprKind::Cast { target, operand } => {
                format!("(cast {} {})", dump_type(target), dump(operand))
            }
            ExprKind::ObjectCreation {
                target,
                arguments,
                initializer,
            } => {
                let mut text = format!("(new {}{}", dump_type(target), dump_args(arguments));
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
            ExprKind::Error => String::from("<error>"),
        }
    }

    fn dump_args(arguments: &[Expr]) -> String {
        let mut text = String::new();
        for argument in arguments {
            text.push(' ');
            text.push_str(&dump(argument));
        }
        text
    }

    /// Renders a type reference, element type first, which matches C# surface
    /// order for the single-rank and jagged cases the tests use.
    fn dump_type(ty: &TypeRef) -> String {
        match &ty.kind {
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
            TypeRefKind::Generic { parts, arguments } => {
                let mut text = parts.join(".");
                text.push('<');
                for (index, argument) in arguments.iter().enumerate() {
                    if index > 0 {
                        text.push(',');
                    }
                    text.push_str(&dump_type(argument));
                }
                text.push('>');
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
                        format!("(exprs{})", dump_args(expressions))
                    }
                };
                let cond = match condition {
                    Some(condition) => dump(condition),
                    None => String::from("_"),
                };
                let iters = if iterators.is_empty() {
                    String::from("_")
                } else {
                    format!("(iters{})", dump_args(iterators))
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
    fn a_missing_type_is_cs1031() {
        assert_eq!(codes("typeof()"), vec![1031]);
        assert_eq!(codes("x is"), vec![1031]);
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
            "(switch x (section (case 1) (expr (call f)) (break)) \
             (section (default) (expr (call g)) (break)))"
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
                        text.push_str(&dump(argument));
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
                    text.push_str(&format!(" {}", dump_accessor("set", setter)));
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
                ..
            } => {
                let mut text = String::from("(indexer");
                for modifier in modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                text.push_str(&format!(" {} [{}]", dump_type(ty), dump_params(parameters)));
                if let Some(getter) = getter {
                    text.push_str(&format!(" {}", dump_accessor("get", getter)));
                }
                if let Some(setter) = setter {
                    text.push_str(&format!(" {}", dump_accessor("set", setter)));
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
        let mut text = format!("({keyword}");
        for modifier in &declaration.modifiers {
            text.push_str(&format!(" {}", modifier_name(*modifier)));
        }
        text.push_str(&format!(" {}", declaration.name));
        if !declaration.bases.is_empty() {
            text.push_str(" :");
            for base in &declaration.bases {
                text.push_str(&format!(" {}", dump_type(base)));
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
                    " {} {} ({})",
                    dump_type(&declaration.return_type),
                    declaration.name,
                    dump_params(&declaration.parameters)
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
                    text.push_str(&dump(element));
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

        assert_eq!(unit_codes("class C { required int f; }"), [8022]);
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
        assert_eq!(unit_codes("namespace N; class C { }"), [8022]);
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
        assert_eq!(unit_codes("public class C<T> { }"), [8022]);
        assert_eq!(unit_codes("public class E { public T M<T>(T x) { return x; } }"), [8022]);
        assert_eq!(unit_codes("class D { System.Collections.Generic.List<int> f; }"), [8022]);

        assert_eq!(unit_codes("public class C<T> : B { void M() { } int F; }"), [8022]);
        assert_eq!(unit_codes("public class C<T, U> { }"), [8022]);
        assert_eq!(unit_codes("class D { System.Collections.Generic.List<System.Collections.Generic.List<int>> f; }"), [8022]);

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
        assert_eq!(unit_codes("public class C<T> { }"), [8022]);

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

        assert_eq!(unit_codes("class C { Box<int> f; }"), [8022]);
        assert_eq!(field_type_at("class C { Box<int> f; }", LanguageVersion::CSharp1), "Box");
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
        assert_eq!(unit_codes("class C { void M() { a<b>>c; } }"), [8022]);

        assert_eq!(
            unit_tree_at("class C { void M() { a<b<c>> d; } }", v2),
            "(class C (method void M () (block (local a<b<c>> d))))"
        );
    }

    #[test]
    fn post_1_0_features_report_cs8022() {
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
            [8022]
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
            unit_tree("class C : N.I { int N.I.M() { return 1; } }"),
            "(class C : N.I (method int N.I.M () (block (return 1))))"
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
        let source = "using System; namespace Hello { class Program { \
                      static void Main() { System.Console.WriteLine(\"Hi\"); } } }";
        assert_eq!(
            unit_tree(source),
            "(using System) (namespace Hello (class Program (method static void Main () \
             (block (expr (call (. (. System Console) WriteLine) str))))))"
        );
    }

    #[test]
    fn declaration_diagnostics_match_the_reference_compiler() {
        assert_eq!(unit_codes("class C { int x }"), vec![1002]);
        assert_eq!(unit_codes("class C {"), vec![1513]);
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
            "using System; namespace N { class C : B { public int F = 0; void M(ref int a) \
             { for (int i = 0; i < 10; i++) { f(i); } } C() : base() {} int P { get; set; } \
             int this[int i] { get { return 0; } } } }",
            "[Serializable] enum E : byte { A, B = 2, } delegate int D(string s);",
            "class C { public static C operator +(C a, C b) { return a; } ~C() {} \
             event H E { add {} remove {} } int[] xs = { 1, 2, 3 }; }",
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
