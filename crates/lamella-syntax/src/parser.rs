//! The parser: building a syntax tree from the token stream.

use crate::ast::{
    Accessor, AssignmentOperator, Attribute, AttributeArgument, AttributeSection, BinaryOperator,
    CatchClause, CompilationUnit, ConstructorInitializer, ConstructorInitializerKind,
    ConversionDirection, DelegateDecl, EnumDecl, EnumMember, Expr, ExprKind, ForInitializer,
    GotoTarget, Literal, Member, Modifier, NamespaceDecl, NamespaceMember, OverloadableOperator,
    Parameter, ParameterModifier, PostfixOperator, PredefinedType, QualifiedName, Stmt, StmtKind,
    SwitchLabel, SwitchSection, TypeDecl, TypeKind, TypeRef, TypeRefKind, TypeTestOperation,
    UnaryOperator, UsingDirective, UsingKind, UsingResource, VariableDeclarator,
};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::lexer::{Tokenized, tokenize};
use crate::span::Span;
use crate::token::{Keyword, Punctuator, Token, TokenKind};
use alloc::boxed::Box;
use alloc::vec::Vec;

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
        diagnostics: parser.diagnostics,
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
        diagnostics: parser.diagnostics,
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

/// Lexes and parses `source` as a whole compilation unit (ECMA-334 1st ed, 16.1).
#[must_use]
pub fn parse_compilation_unit(source: &str) -> ParsedCompilationUnit {
    let mut parser = Parser::new(tokenize(source));
    let unit = parser.parse_compilation_unit();
    ParsedCompilationUnit {
        unit,
        diagnostics: parser.diagnostics,
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
        diagnostics: parser.diagnostics,
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
                types.push(self.parse_namespace_member());
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
        let saved_position = self.position;
        let saved_diagnostics = self.diagnostics.len();
        let ty = self.parse_type();
        if !matches!(ty.kind, TypeRefKind::Error)
            && matches!(self.current().kind, TokenKind::Identifier(_))
        {
            return self.parse_local_declaration(start, ty);
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
    fn parse_local_declaration(&mut self, start: u32, ty: TypeRef) -> Stmt {
        let declarators = self.parse_variable_declarators();
        let end = self.expect(Punctuator::Semicolon, DiagnosticKind::SemicolonExpected);
        Stmt::new(
            StmtKind::LocalDeclaration { ty, declarators },
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
        self.bump();
        let (exception_type, name) = if self.eat(Punctuator::OpenParen) {
            let ty = self.parse_type();
            let name = if matches!(self.current().kind, TokenKind::Identifier(_)) {
                Some(self.expect_identifier().0)
            } else {
                None
            };
            self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
            (Some(ty), name)
        } else {
            (None, None)
        };
        let body = Box::new(self.parse_required_block());
        CatchClause {
            exception_type,
            name,
            body,
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
        let (name, _) = self.expect_identifier();
        self.expect(Punctuator::Equals, DiagnosticKind::TokenExpected { expected: "=" });
        let init = self.parse_expression();
        self.expect(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
        let body = Box::new(self.parse_statement());
        let end = body.span.end;
        Stmt::new(
            StmtKind::Fixed {
                ty,
                name,
                init,
                body,
            },
            Span::new(start, end),
        )
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
            while self.current_keyword() != Some(Keyword::Case)
                && self.current_keyword() != Some(Keyword::Default)
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
            Some(Keyword::Default) => {
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
        while !matches!(self.current().kind, TokenKind::EndOfFile) {
            let before = self.position;
            members.push(self.parse_namespace_member());
            if self.position == before {
                self.bump();
            }
        }
        let end = self.current().span.start;
        CompilationUnit {
            usings,
            members,
            span: Span::new(start, end),
        }
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
        let target = if matches!(self.current().kind, TokenKind::Identifier(_))
            && self.next_is(Punctuator::Colon)
        {
            let (target, _) = self.expect_identifier();
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
    fn parse_namespace_member(&mut self) -> NamespaceMember {
        let start = self.current().span.start;
        let attributes = self.parse_attribute_sections();
        if self.current_keyword() == Some(Keyword::Namespace) {
            return NamespaceMember::Namespace(self.parse_namespace_declaration());
        }
        let modifiers = self.parse_modifiers();
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
                || !matches!(self.current().kind, TokenKind::Identifier(_))
            {
                break;
            }
            let member_start = self.current().span.start;
            let (member_name, mut member_end) = self.expect_identifier();
            let value = if self.eat(Punctuator::Equals) {
                let value = self.parse_expression();
                member_end = value.span.end;
                Some(value)
            } else {
                None
            };
            members.push(EnumMember {
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
        let parameters = self.parse_parameter_list();
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

    /// Parses a `namespace` declaration (16.2).
    fn parse_namespace_declaration(&mut self) -> NamespaceDecl {
        let start = self.current().span.start;
        self.bump();
        let name = self.parse_qualified_name();
        self.expect(Punctuator::OpenBrace, DiagnosticKind::OpenBraceExpected);
        let usings = self.parse_using_directives();
        let mut members = Vec::new();
        while self.current_punctuator() != Some(Punctuator::CloseBrace)
            && !matches!(self.current().kind, TokenKind::EndOfFile)
        {
            let before = self.position;
            members.push(self.parse_namespace_member());
            if self.position == before {
                self.bump();
            }
        }
        let end = self.expect(Punctuator::CloseBrace, DiagnosticKind::CloseBraceExpected);
        NamespaceDecl {
            name,
            usings,
            members,
            span: Span::new(start, end),
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
            bases,
            members,
            span: Span::new(start, end),
        }
    }

    /// Parses a run of leading declaration modifiers (17.2 and elsewhere). The
    /// parser accepts any; binding checks which are valid where.
    fn parse_modifiers(&mut self) -> Vec<Modifier> {
        let mut modifiers = Vec::new();
        while let Some(modifier) = self.current_keyword().and_then(modifier_of) {
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
            let parameters = self.parse_parameter_list();
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
                initializer,
                body,
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
            && self.next_is(Punctuator::OpenParen)
        {
            let (name, _) = self.expect_identifier();
            let parameters = self.parse_parameter_list();
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
                parameters,
                body,
                explicit_interface: None,
                attributes: Vec::new(),
                span: Span::new(start, end),
            };
        }
        if matches!(self.current().kind, TokenKind::Identifier(_)) && self.next_is(Punctuator::Dot) {
            let name_start = self.current().span.start;
            let (first, mut prev_end) = self.expect_identifier();
            let mut parts = Vec::new();
            parts.push(first);
            let mut interface_end = prev_end;
            while self.current_punctuator() == Some(Punctuator::Dot) {
                self.bump();
                interface_end = prev_end;
                let (part, part_end) = self.expect_identifier();
                parts.push(part);
                prev_end = part_end;
            }
            let member = parts.pop().expect("a qualified member name has >= 2 parts");
            let explicit_interface = TypeRef::new(
                TypeRefKind::Name(parts),
                Span::new(name_start, interface_end),
            );
            if self.current_punctuator() == Some(Punctuator::OpenBrace) {
                return self.parse_property(modifiers, ty, member, Some(explicit_interface), start);
            }
            let parameters = self.parse_parameter_list();
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
                parameters,
                body,
                explicit_interface: Some(explicit_interface),
                attributes: Vec::new(),
                span: Span::new(start, end),
            };
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
        let at = self.current().span.start;
        self.report(DiagnosticKind::IdentifierExpected, Span::empty_at(at));
        Member::Error
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
        let parameters = self.parse_parameter_list();
        let body = self.parse_required_block();
        let end = body.span.end;
        Member::Operator {
            modifiers,
            return_type,
            operator,
            parameters,
            body,
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
        let parameters = self.parse_parameter_list();
        let body = self.parse_required_block();
        let end = body.span.end;
        Member::ConversionOperator {
            modifiers,
            direction,
            target,
            parameters,
            body,
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
            span: Span::new(start, end),
        }
    }

    /// Parses a constructor initializer (17.10.1), the scanner just past the `:`:
    /// `base ( args )` or `this ( args )`.
    fn parse_constructor_initializer(&mut self) -> ConstructorInitializer {
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
        ConstructorInitializer { kind, arguments }
    }

    /// Parses an indexer given the modifiers and type already parsed (17.8): the
    /// `this` keyword, a bracketed index parameter list, then an accessor body.
    fn parse_indexer(&mut self, modifiers: Vec<Modifier>, ty: TypeRef, start: u32) -> Member {
        self.bump();
        self.expect(
            Punctuator::OpenBracket,
            DiagnosticKind::TokenExpected { expected: "[" },
        );
        let parameters = self.parse_parameter_sequence(Punctuator::CloseBracket);
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
            span: Span::new(start, end),
        }
    }

    /// Parses a parenthesized formal-parameter list (17.5.1).
    fn parse_parameter_list(&mut self) -> Vec<Parameter> {
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
    /// indexer index lists `[ ]`.
    fn parse_parameter_sequence(&mut self, close: Punctuator) -> Vec<Parameter> {
        let mut parameters = Vec::new();
        if self.current_punctuator() == Some(close) {
            return parameters;
        }
        loop {
            let start = self.current().span.start;
            let _ = self.parse_attribute_sections();
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
            let (name, end) = self.expect_identifier();
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
        parameters
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
                    let target = self.parse_type();
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
                let target = self.parse_type();
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
            if self.position == before {
                break;
            }
            break;
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
        let mut base = self.parse_non_array_type();
        let start = base.span.start;
        while !matches!(base.kind, TypeRefKind::Error)
            && self.current_punctuator() == Some(Punctuator::Asterisk)
        {
            let end = self.current().span.end;
            self.bump();
            base = TypeRef::new(TypeRefKind::Pointer(Box::new(base)), Span::new(start, end));
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
        let element = self.parse_non_array_type();
        match self.current_punctuator() {
            Some(Punctuator::OpenParen) => {
                self.bump();
                let (arguments, end) = self
                    .parse_arguments(Punctuator::CloseParen, DiagnosticKind::CloseParenExpected);
                Expr::new(
                    ExprKind::ObjectCreation {
                        target: element,
                        arguments,
                    },
                    Span::new(start, end),
                )
            }
            Some(Punctuator::OpenBracket) => self.parse_array_creation(start, element),
            _ => {
                let end = self.expect(
                    Punctuator::OpenParen,
                    DiagnosticKind::TokenExpected { expected: "(" },
                );
                Expr::new(
                    ExprKind::ObjectCreation {
                        target: element,
                        arguments: Vec::new(),
                    },
                    Span::new(start, end),
                )
            }
        }
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

    /// Parses a type name (11.1): `identifier ('.' identifier)*`.
    fn parse_type_name(&mut self) -> TypeRef {
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
        TypeRef::new(TypeRefKind::Name(parts), Span::new(start, end))
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
            ExprKind::Invocation {
                receiver,
                arguments,
            } => format!("(call {}{})", dump(receiver), dump_args(arguments)),
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
            ExprKind::StackAlloc { element, count } => {
                format!("(stackalloc {} {})", dump_type(element), dump(count))
            }
            ExprKind::Dereference(operand) => format!("(deref {})", dump(operand)),
            ExprKind::Checked(inner) => format!("(checked {})", dump(inner)),
            ExprKind::Unchecked(inner) => format!("(unchecked {})", dump(inner)),
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
            ExprKind::ObjectCreation { target, arguments } => {
                format!("(new {}{})", dump_type(target), dump_args(arguments))
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
            StmtKind::LocalDeclaration { ty, declarators } => {
                let mut text = format!("(local {}", dump_type(ty));
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
                    " {} {qualified} ({})",
                    dump_type(return_type),
                    dump_params(parameters)
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
                initializer,
                body,
                ..
            } => {
                let mut text = String::from("(ctor");
                for modifier in modifiers {
                    text.push_str(&format!(" {}", modifier_name(*modifier)));
                }
                text.push_str(&format!(" {name} ({})", dump_params(parameters)));
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
                let mut text = format!("(namespace {}", dump_qname(&declaration.name));
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
}
