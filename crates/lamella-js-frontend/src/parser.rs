//! Recursive descent over ECMAScript, with the goal symbol supplied at every token request.

use crate::{format, vec, Box, String, ToString, Vec};
use crate::ast::*;
use crate::context::Context;
use crate::diagnostic::{Diagnostic, DiagnosticKind, Diagnostics, Phase};
use crate::lexer::Lexer;
use crate::source::Span;
use crate::string_value::JsString;
use crate::token::{Goal, Punctuator, TemplateKind, Token, TokenKind};

/// What a parse produced: a tree, and everything wrong with it.
pub struct Parsed {
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
    /// Cascades dropped. Reported so an over-aggressive suppression rule is visible rather than
    /// silently hiding findings.
    pub suppressed: usize,
}

impl Parsed {
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }

    /// The phase of the first error, which is what a `negative` test asserts.
    #[must_use]
    pub fn first_error_phase(&self) -> Option<Phase> {
        self.diagnostics.iter().find(|d| d.is_error()).map(|d| d.phase)
    }
}

/// Parses a whole script.
pub fn parse_script(source: &str) -> Parsed {
    let mut parser = Parser::new(source);
    let program = parser.parse_program(Context::script());
    crate::early_errors::check(&program, &mut parser.diagnostics);
    Parsed {
        program,
        suppressed: parser.diagnostics.suppressed(),
        diagnostics: parser.diagnostics.into_items(),
    }
}

/// A saved position, for speculative parsing.
#[derive(Clone, Copy)]
struct Snapshot<'a> {
    lexer: Lexer<'a>,
    current: usize,
}

pub struct Parser<'a> {
    source: &'a str,
    lexer: Lexer<'a>,
    /// The token in hand. Exactly one is buffered, and it is re-fetched when the goal changes.
    token: Token,
    /// Where `token` was scanned from, so a re-lex at another goal starts in the right place.
    token_start: usize,
    diagnostics: Diagnostics,
    /// Complaints about object literals that a REFINEMENT would cancel, parked until it is known
    /// whether one happens.
    ///
    /// # A PARKED COMPLAINT, WHICH IS WHAT A COVER GRAMMAR NEEDS
    ///
    /// `{a = 1}` is an early error as an OBJECT LITERAL and legal as a destructuring PATTERN, and
    /// which it is depends on a token that has not arrived. Raising the error where it is seen
    /// rejects every `({a = 1} = b)`; never raising it accepts a form the standard explicitly bars.
    /// So it is parked, refinement CANCELS it, and whatever is still parked at the end was never
    /// refined and is reported then.
    parked_cover_errors: Vec<(Span, &'static str)>,
    /// Whether the statement about to be parsed sits in a position that admits a *Statement* and
    /// not a *Declaration* -- the body of an `if`, a loop, a `with`, or a label.
    ///
    /// # IT CHANGES WHAT `let` IS, NOT ONLY WHETHER A DECLARATION IS ALLOWED
    ///
    /// A *StatementList* position has both productions, so `let` followed by an identifier is a
    /// LexicalDeclaration and the question of ExpressionStatement never arises. A Statement-only
    /// position has no LexicalDeclaration **at all**, so the same tokens are an ExpressionStatement
    /// whose subject is the identifier `let` -- and `if (false) let \n x = 1;` is two statements,
    /// `let;` and `x = 1;`, joined by automatic semicolon insertion. This parser read the
    /// declaration either way and refused it, which rejects a program the language accepts.
    ///
    /// The one spelling that stays an error is `let [`, and it is an error for the OTHER reason:
    /// ExpressionStatement's lookahead restriction bars it, and with no LexicalDeclaration to fall
    /// back on nothing matches at all.
    ///
    /// **TAKEN BY `parse_statement` ON ENTRY**, so it describes one statement and cannot leak
    /// into the statements nested inside it -- `if (a) { let x = 1; }` is a StatementList again the
    /// moment the block opens.
    only_a_statement: bool,
}

impl<'a> Parser<'a> {
    #[must_use]
    pub fn new(source: &'a str) -> Self {
        let mut parser = Self {
            source,
            lexer: Lexer::new(source),
            token: Token {
                kind: TokenKind::EndOfFile,
                span: Span::new(0, 0),
                preceded_by_line_terminator: false,
                had_escape: false,
            },
            token_start: 0,
            diagnostics: Diagnostics::new(),
            parked_cover_errors: Vec::new(),
            only_a_statement: false,
        };
        parser.advance(Goal::RegExp);
        parser
    }


    /// Scans the next token at `goal`.
    ///
    /// A lexical error becomes a diagnostic in the shared channel and the parser **keeps going**
    /// from after the offending text, rather than unwinding. One bad escape must not cost the rest
    /// of the file, which is the whole reason the channel accumulates.
    fn advance(&mut self, goal: Goal) {
        self.token_start = self.lexer.offset();
        loop {
            match self.lexer.next_token(goal) {
                Ok(token) => {
                    self.token = token;
                    return;
                }
                Err(error) => {
                    let excluded = matches!(
                        error.kind,
                        crate::lexer::LexErrorKind::BigIntNotInProfile
                            | crate::lexer::LexErrorKind::LegacyOctal
                    );
                    let placeholder = TokenKind::Number(0.0);
                    let kind =
                        if excluded { DiagnosticKind::NotInProfile } else { DiagnosticKind::Lexical };
                    self.diagnostics.error(
                        Phase::Lexical,
                        kind,
                        error.span,
                        format!("{:?}", error.kind),
                    );

                    if excluded {
                        self.lexer.reset_to(error.span.end.min(self.source.len()));
                        self.token = Token {
                            kind: placeholder,
                            span: error.span,
                            preceded_by_line_terminator: false,
                            had_escape: false,
                        };
                        return;
                    }
                    let candidate = error.span.end.max(self.lexer.offset() + 1);
                    let next = next_char_boundary(self.source, candidate);
                    if next >= self.source.len() {
                        self.token = Token {
                            kind: TokenKind::EndOfFile,
                            span: Span::new(self.source.len(), self.source.len()),
                            preceded_by_line_terminator: false,
                            had_escape: false,
                        };
                        return;
                    }
                    self.lexer.reset_to(next);
                }
            }
        }
    }

    /// Re-scans the token in hand at a different goal.
    ///
    /// The operation the goal-symbol design exists for: the parser learns from context that the `/`
    /// it is looking at opens a RegExp, and asks again from the same position.
    fn relex(&mut self, goal: Goal) {
        self.lexer.reset_to(self.token_start);
        self.advance(goal);
    }

    fn snapshot(&self) -> Snapshot<'a> {
        Snapshot { lexer: self.lexer, current: self.token_start }
    }

    fn restore(&mut self, snapshot: Snapshot<'a>, goal: Goal) {
        self.lexer = snapshot.lexer;
        self.lexer.reset_to(snapshot.current);
        self.advance(goal);
    }

    fn at(&self, punctuator: Punctuator) -> bool {
        self.token.is_punctuator(punctuator)
    }

    fn at_keyword(&self, keyword: &str) -> bool {
        self.token.keyword_text() == Some(keyword)
    }

    fn at_end(&self) -> bool {
        self.token.kind == TokenKind::EndOfFile
    }

    fn eat(&mut self, punctuator: Punctuator, goal: Goal) -> bool {
        if self.at(punctuator) {
            self.advance(goal);
            true
        } else {
            false
        }
    }

    fn eat_keyword(&mut self, keyword: &str, goal: Goal) -> bool {
        if self.at_keyword(keyword) {
            self.advance(goal);
            true
        } else {
            false
        }
    }

    fn expect(&mut self, punctuator: Punctuator, goal: Goal) {
        if !self.eat(punctuator, goal) {
            self.error(
                DiagnosticKind::MissingToken,
                format!("expected `{}`", punctuator.spelling()),
            );
        }
    }

    fn error(&mut self, kind: DiagnosticKind, message: impl Into<String>) {
        self.diagnostics.error(Phase::Syntactic, kind, self.token.span, message);
    }

    fn early_error(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics.error(Phase::Early, DiagnosticKind::EarlyError, span, message);
    }

    /// The early errors on a `RegularExpressionLiteral`: the body must parse and the flags must be
    /// legal and distinct.
    ///
    /// # THE PHASE IS THE WHOLE POINT, AND DEFERRING IT IS OBSERVABLE
    ///
    /// The lexer scans a literal's body and flags as TEXT and validates neither, so a bad flag
    /// parses cleanly and throws when the pattern is compiled -- at RUN time, with the program
    /// already running. 22.2.1 makes both checks EARLY ERRORS on the literal, so a conforming
    /// program never runs at all, and a negative test asserts the phase rather than the message.
    ///
    /// # A PUBLISHED ABSENCE IS NOT A SYNTAX ERROR, AND THE CRATE ALREADY KNOWS THE DIFFERENCE
    ///
    /// `\p{...}` is legal ECMAScript this engine does not implement, and the published list records
    /// it as refusing at RUN time. Raising it here would move it to parse time and make that list
    /// wrong about its own phase. So only an error that is NOT a published absence is reported:
    /// `ErrorKind::absence` draws that line, and it exists so a consumer's list can be built from
    /// it.
    fn check_regexp_literal(&mut self, body: &str, flags: &str, span: Span) {
        let parsed = match lamella_regexp::js::Flags::parse(flags) {
            Ok(parsed) => parsed,
            Err(error) => {
                if error.kind.absence().is_none() {
                    self.early_error(span, error.kind.message());
                }
                return;
            }
        };
        if let Err(error) = lamella_regexp::js::parse(body, parsed) {
            if error.kind.absence().is_none() {
                self.early_error(span, error.kind.message());
            }
        }
    }

    fn span_from(&self, start: usize) -> Span {
        Span::new(start, self.token_start.max(start))
    }


    pub fn parse_program(&mut self, context: Context) -> Program {
        let start = self.token.span.start;
        let (body, strict, _) = self.parse_directive_prologue_and_body(context, None);
        for (span, message) in core::mem::take(&mut self.parked_cover_errors) {
            self.early_error(span, message);
        }
        Program { body, strict, span: Span::new(start, self.source.len()) }
    }

    /// Parses a directive prologue and then statements until `terminator`.
    ///
    /// Returns the body, whether it IS strict, and whether it DECLARED itself strict.
    ///
    /// # THE DIRECTIVE RULE IS DELIBERATELY PEDANTIC AND TEST262 CHECKS IT
    ///
    /// `"use strict"` counts only in the prologue -- the leading run of expression statements that
    /// are single string literals -- and only if the literal contains **no escape sequence**.
    /// `"use \x73trict"` is an ordinary statement with no effect. A parser that compares the cooked
    /// value turns that into strict mode and changes what the rest of the file means.
    ///
    /// # WARNING: THOSE LAST TWO ARE DIFFERENT AND ONE RULE TURNS ON THE DIFFERENCE
    ///
    /// A function inside strict code IS strict without saying anything. The non-simple-parameter
    /// rule fires only on a body that CONTAINS a `"use strict"` directive, so
    /// `'use strict'; function f(a = 1) {}` is legal and `'use strict'; function f(a = 1) { 'use
    /// strict'; }` is not. A check keyed on "is strict" gets the first one wrong; one keyed on the
    /// enclosing mode being sloppy gets the second one wrong. Both facts have to be returned.
    fn parse_directive_prologue_and_body(
        &mut self,
        mut context: Context,
        terminator: Option<Punctuator>,
    ) -> (Vec<Statement>, bool, bool) {
        let mut body = Vec::new();
        let mut strict = context.strict;
        let mut declared_strict = false;
        let mut in_prologue = true;

        loop {
            if self.at_end() {
                break;
            }
            if let Some(terminator) = terminator {
                if self.at(terminator) {
                    break;
                }
            }

            if in_prologue {
                if let Some(raw) = self.directive_text() {
                    let is_use_strict = raw == "\"use strict\"" || raw == "'use strict'";
                    let statement = self.parse_statement(context);
                    if is_use_strict {
                        strict = true;
                        declared_strict = true;
                        context = context.strict();
                    }
                    body.push(statement);
                    continue;
                }
                in_prologue = false;
            }

            let before = self.token_start;
            body.push(self.parse_statement(context));
            if self.token_start == before && !self.at_end() {
                self.advance(Goal::RegExp);
            }
        }
        (body, strict, declared_strict)
    }

    /// The raw source of the token in hand if it is a lone string literal statement.
    ///
    /// Returns the **raw text**, not the cooked value, because the escape rule above turns on the
    /// spelling rather than on what it denotes.
    fn directive_text(&self) -> Option<&'a str> {
        if !matches!(self.token.kind, TokenKind::String(_)) {
            return None;
        }
        Some(self.token.span.text(self.source))
    }

    fn parse_statement(&mut self, context: Context) -> Statement {
        let start = self.token.span.start;
        let only_a_statement = core::mem::take(&mut self.only_a_statement);

        if self.at(Punctuator::OpenBrace) {
            self.advance(Goal::RegExp);
            let body = self.parse_statement_list(context, Punctuator::CloseBrace);
            self.expect(Punctuator::CloseBrace, Goal::Div);
            return Statement::Block { body, span: self.span_from(start) };
        }
        if self.eat(Punctuator::Semicolon, Goal::RegExp) {
            return Statement::Empty { span: self.span_from(start) };
        }

        if let Some(keyword) = self.token.keyword_text() {
            match keyword {
                "var" => return self.parse_declaration(context, DeclarationKind::Var, start),
                "const" => return self.parse_declaration(context, DeclarationKind::Const, start),
                "let" if self.let_begins_a_declaration_in(only_a_statement) => {
                    return self.parse_declaration(context, DeclarationKind::Let, start)
                }
                "if" => return self.parse_if(context, start),
                "while" => return self.parse_while(context, start),
                "do" => return self.parse_do_while(context, start),
                "for" => return self.parse_for(context, start),
                "return" => return self.parse_return(context, start),
                "break" | "continue" => return self.parse_break_or_continue(keyword == "break", start),
                "throw" => return self.parse_throw(context, start),
                "try" => return self.parse_try(context, start),
                "switch" => return self.parse_switch(context, start),
                "function" => {
                    let function = self.parse_function(context, start, false);
                    return Statement::Function(Box::new(function));
                }
                "debugger" => {
                    self.advance(Goal::RegExp);
                    self.consume_semicolon();
                    return Statement::Debugger { span: self.span_from(start) };
                }
                "with" => return self.parse_with(context, start),
                "class" => return Statement::Class(Box::new(self.parse_class(context))),
                _ => {}
            }
        }

        if matches!(self.token.kind, TokenKind::Identifier(_)) {
            let saved = self.snapshot();
            if let TokenKind::Identifier(name) = self.token.kind.clone() {
                self.advance(Goal::Div);
                if self.eat(Punctuator::Colon, Goal::RegExp) {
                    if reserved_in(&name, context) {
                        self.error(
                            DiagnosticKind::UnexpectedToken,
                            format!("`{name}` is a reserved word and may not label a statement"),
                        );
                    }
                    let body = self.parse_label_body(context);
                    return Statement::Labeled {
                        label: name,
                        body: Box::new(body),
                        span: self.span_from(start),
                    };
                }
                self.restore(saved, Goal::RegExp);
            }
        }

        let expression = self.parse_expression(context);
        self.consume_semicolon();
        Statement::Expression { expression, span: self.span_from(start) }
    }

    /// [`Self::let_begins_a_declaration`], narrowed for a position that admits no declaration.
    ///
    /// IN A STATEMENT-ONLY POSITION THE ANSWER IS `let [` AND NOTHING ELSE, because there is no
    /// LexicalDeclaration production to choose. `if (false) let \n x = 1;` is an ExpressionStatement
    /// holding the identifier `let`, closed by automatic semicolon insertion, followed by an
    /// ordinary assignment -- and a conforming engine runs it. `let [` stays an error, not because
    /// a declaration is refused there but because ExpressionStatement's own lookahead restriction
    /// excludes it and nothing else in the grammar matches. See [`Self::only_a_statement`].
    fn let_begins_a_declaration_in(&self, only_a_statement: bool) -> bool {
        if only_a_statement {
            return self.let_is_followed_by_an_open_bracket();
        }
        self.let_begins_a_declaration()
    }

    /// The `let [` half of the rule on its own: the one spelling that is barred wherever it appears.
    fn let_is_followed_by_an_open_bracket(&self) -> bool {
        let mut probe = self.lexer;
        probe.reset_to(self.token.span.end);
        matches!(probe.next_token(Goal::Div), Ok(next) if next.is_punctuator(Punctuator::OpenBracket))
    }

    /// The spec's lookahead restriction for statement-position `let`.
    ///
    /// `let [` is always a declaration; `let` followed by an identifier or `{` is one too. Anything
    /// else -- `let = 1`, `let.x`, `let(0)` -- is the identifier `let`.
    fn let_begins_a_declaration(&self) -> bool {
        let mut probe = self.lexer;
        probe.reset_to(self.token.span.end);
        match probe.next_token(Goal::Div) {
            Ok(next) => match &next.kind {
                TokenKind::Identifier(name) => {
                    !ALWAYS_RESERVED.contains(&name.as_str()) || next.had_escape
                }
                _ => next.is_punctuator(Punctuator::OpenBracket)
                    || next.is_punctuator(Punctuator::OpenBrace),
            },
            Err(_) => false,
        }
    }

    /// Parses the body of a LABEL, which is the one nested position that admits a declaration.
    ///
    /// `LabelledItem : Statement | FunctionDeclaration`, so `lbl: function f() {}` is legal in sloppy
    /// code -- and so is `a: b: c: function f() {}`, because each label's body is itself a
    /// LabelledStatement all the way down.
    ///
    /// # WARNING: SO THE IsLabelledFunction CHECK MUST NOT RUN HERE
    ///
    /// That check exists to stop a label smuggling a function into an `if` or a loop body. Running it
    /// on a label's OWN body rejects the legal nested-label form, which is what the first version
    /// did: it saw `b: c: function f() {}` as "a labelled function" and complained, at the top level
    /// of a script, where the whole construct is allowed.
    fn parse_label_body(&mut self, context: Context) -> Statement {
        if !context.strict && self.at_keyword("function") {
            return self.parse_statement(context);
        }
        if let Some(what) = self.declaration_starts_here(true) {
            let span = self.token.span;
            self.early_error(span, format!("a {what} declaration may not be labelled"));
        }
        self.only_a_statement = true;
        self.parse_statement(context)
    }

    /// Parses the body of an `if`, a loop or a `with`.
    ///
    /// # A DECLARATION IS NOT A STATEMENT, AND THAT IS THE WHOLE RULE
    ///
    /// The grammar lets these positions hold a *Statement*, and `let`, `const`, `class` and
    /// `function` declarations are not Statements -- so `if (a) let x = 1;` and
    /// `for (var x of []) let y;` are SyntaxErrors. `var` IS a Statement and stays legal.
    ///
    /// It reads like a technicality and it is the single largest cluster of parse-negatives this
    /// parser was accepting: over a hundred tests across `if`, `for-in`, `for-of` and the loops.
    /// The reason it is easy to miss is that the declaration parses perfectly well -- nothing is
    /// malformed, it is simply not allowed HERE.
    ///
    /// Annex B relaxes the function-declaration case for sloppy code in browsers. Annex B is
    /// excluded from this profile, so we reject, and that is consistent rather than accidental.
    /// AND `let` IS THE EXCEPTION, because in this position it may not be a declaration at all --
    /// see [`Self::only_a_statement`]. `if (a) let \n x = 1;` is two statements and runs.
    fn parse_nested_statement(&mut self, context: Context) -> Statement {
        if let Some(what) = self.declaration_starts_here(true) {
            let span = self.token.span;
            if what == "function" && !context.strict {
                self.error(
                    DiagnosticKind::NotInProfile,
                    "a function declaration here is Annex B, which is not in this profile",
                );
            } else {
                self.early_error(
                    span,
                    format!("a {what} declaration may not be the body of a statement"),
                );
            }
        }
        self.only_a_statement = true;
        let statement = self.parse_statement(context);
        if is_labelled_function(&statement) {
            self.early_error(
                statement.span(),
                "a labelled function declaration may not be the body of a statement",
            );
        }
        statement
    }

    /// What kind of declaration begins here, if any. `var` is deliberately absent: it IS a Statement.
    fn declaration_starts_here(&self, only_a_statement: bool) -> Option<&'static str> {
        match self.token.keyword_text()? {
            "const" => Some("lexical"),
            "let" if self.let_begins_a_declaration_in(only_a_statement) => Some("lexical"),
            "class" => Some("class"),
            "function" => Some("function"),
            "async" if self.async_function_follows() => Some("async function"),
            _ => None,
        }
    }

    fn parse_statement_list(&mut self, context: Context, terminator: Punctuator) -> Vec<Statement> {
        let mut body = Vec::new();
        while !self.at_end() && !self.at(terminator) {
            let before = self.token_start;
            body.push(self.parse_statement(context));
            if self.token_start == before && !self.at_end() {
                self.advance(Goal::RegExp);
            }
        }
        body
    }

    /// Consumes a statement terminator, applying automatic semicolon insertion.
    ///
    /// # The three insertion rules, and the one that is not about `;` at all
    ///
    /// A semicolon is inserted when the offending token is **preceded by a line terminator**, at
    /// **end of input**, or before a **`}`**. The token's flag is the only input, which is why the
    /// scanner records it -- by the time this runs, the whitespace is long gone.
    fn consume_semicolon(&mut self) {
        if self.eat(Punctuator::Semicolon, Goal::RegExp) {
            return;
        }
        if self.at_end() || self.at(Punctuator::CloseBrace) || self.token.preceded_by_line_terminator
        {
            return;
        }
        self.error(DiagnosticKind::MissingSemicolon, "expected `;` or a line break");
    }

    fn parse_declaration(
        &mut self,
        context: Context,
        kind: DeclarationKind,
        start: usize,
    ) -> Statement {
        self.advance(Goal::RegExp);
        let declarations = self.parse_declarator_list(context, kind, true);
        self.consume_semicolon();
        Statement::Declaration { kind, declarations, span: self.span_from(start) }
    }

    /// Parses `a = 1, b = 2`.
    ///
    /// # `require_initializer` IS A PARAMETER BECAUSE THE ANSWER ARRIVES LATER
    ///
    /// `const x;` is an early error and `for (const x of xs)` is perfectly legal -- and which one
    /// this is depends on a token that has not been read yet when the declarator is parsed. So the
    /// check is deferred to the caller that finds out, exactly like a cover grammar's refinement.
    /// Applying it here unconditionally rejects every `for (const ... of ...)` in existence.
    fn parse_declarator_list(
        &mut self,
        context: Context,
        kind: DeclarationKind,
        require_initializer: bool,
    ) -> Vec<Declarator> {
        let mut declarations = Vec::new();
        loop {
            let start = self.token.span.start;
            let target = self.parse_binding_target(context);
            let init = if self.eat(Punctuator::Equals, Goal::RegExp) {
                Some(self.parse_assignment(context))
            } else {
                None
            };
            if require_initializer && kind == DeclarationKind::Const && init.is_none() {
                let span = self.span_from(start);
                self.early_error(span, "a `const` declaration must have an initializer");
            }
            declarations.push(Declarator { target, init, span: self.span_from(start) });
            if !self.eat(Punctuator::Comma, Goal::RegExp) {
                return declarations;
            }
        }
    }

    fn parse_binding_target(&mut self, context: Context) -> Pattern {
        let start = self.token.span.start;
        if self.at(Punctuator::OpenBracket) || self.at(Punctuator::OpenBrace) {
            let literal = self.parse_primary(context);
            return self.refine_to_pattern(literal, context);
        }
        match self.token.kind.clone() {
            TokenKind::Identifier(name) => {
                self.check_binding_identifier(&name, context);
                self.advance(Goal::Div);
                Pattern::Identifier { name, span: self.span_from(start) }
            }
            _ => {
                self.error(DiagnosticKind::UnexpectedToken, "expected a binding target");
                Pattern::Identifier { name: String::new(), span: self.span_from(start) }
            }
        }
    }

    /// Early errors on a binding name.
    fn check_binding_identifier(&mut self, name: &str, context: Context) {
        if reserved_in(name, context) {
            let span = self.token.span;
            self.early_error(span, format!("`{name}` is a reserved word and may not be bound"));
        }
        if context.strict && (name == "eval" || name == "arguments") {
            let span = self.token.span;
            self.early_error(span, format!("`{name}` may not be bound in strict mode"));
        }
        if context.allow_yield && name == "yield" {
            let span = self.token.span;
            self.early_error(span, "`yield` may not be used as a binding name here");
        }
        if context.allow_await && name == "await" {
            let span = self.token.span;
            self.early_error(span, "`await` may not be used as a binding name here");
        }
    }

    /// Parses a `class`, in either the declaration or the expression position.
    ///
    /// **A CLASS BODY IS STRICT WHATEVER SURROUNDS IT, AND IT SAYS SO NOWHERE.** There is no
    /// directive to find: the strictness comes from the production itself. Parsing the body with the
    /// enclosing context would accept `class C { m() { with (a) {} } }` and every other thing strict
    /// mode forbids, in code the standard makes strict by construction.
    ///
    /// **`constructor` IS NOT A METHOD**, it is the class's own callable -- and only the
    /// non-computed, non-static spelling counts. `class C { ["constructor"]() {} }` defines an
    /// ordinary method, and `static constructor() {}` is a static method named `constructor`.
    fn parse_class(&mut self, context: Context) -> Class {
        let start = self.token.span.start;
        self.advance(Goal::Div);
        let context = context.strict();
        let name = match &self.token.kind {
            TokenKind::Identifier(name) if !self.at_keyword("extends") => {
                let name = name.clone();
                self.check_binding_identifier(&name, context);
                self.advance(Goal::Div);
                Some(name)
            }
            _ => None,
        };
        let heritage = if self.eat_keyword("extends", Goal::RegExp) {
            Some(Box::new(self.parse_call_or_member(context)))
        } else {
            None
        };
        self.expect(Punctuator::OpenBrace, Goal::RegExp);
        let mut members = Vec::new();
        while !self.at(Punctuator::CloseBrace) && !self.at_end() {
            if self.eat(Punctuator::Semicolon, Goal::RegExp) {
                continue;
            }
            let member_start = self.token.span.start;
            let is_static = self.at_keyword("static") && !self.next_is_member_terminator();
            if is_static {
                self.advance(Goal::Div);
            }
            if let Some(missing) = self.unsupported_class_member() {
                self.error(
                    DiagnosticKind::NotInProfile,
                    format!("{missing} are not in this profile"),
                );
                self.skip_class_member();
                continue;
            }
            let is_generator = self.eat(Punctuator::Star, Goal::Div);
            let (kind, key, computed) = if !is_generator && self.accessor_here().is_some() {
                let kind = self.accessor_here().unwrap_or(MethodKind::Normal);
                self.advance(Goal::Div);
                let (key, computed) = self.parse_property_key(context);
                (kind, key, computed)
            } else {
                let (key, computed) = self.parse_property_key(context);
                (MethodKind::Normal, key, computed)
            };
            if !self.at(Punctuator::OpenParen) {
                self.error(DiagnosticKind::NotInProfile, "class fields are not in this profile");
                self.skip_class_member();
                continue;
            }
            let body_context = context.function_body(is_generator, false);
            let params_start = self.token.span.start;
            let params = self.parse_parameter_list(body_context, true);
            let params_span = self.span_from(params_start);
            self.expect(Punctuator::OpenBrace, Goal::RegExp);
            let (body, strict, declared_strict) =
                self.parse_directive_prologue_and_body(body_context, Some(Punctuator::CloseBrace));
            self.expect(Punctuator::CloseBrace, Goal::Div);
            self.recheck_parameters_after_body(&params, params_span, strict, declared_strict);
            if kind == MethodKind::Set && params.len() != 1 {
                let span = self.span_from(member_start);
                self.early_error(span, "a setter takes exactly one parameter");
            }
            if kind == MethodKind::Get && !params.is_empty() {
                let span = self.span_from(member_start);
                self.early_error(span, "a getter takes no parameters");
            }
            let mut function = Function {
                name: None,
                params,
                body,
                is_generator,
                is_async: false,
                strict: true,
                span: self.span_from(member_start),
            };
            crate::generator_transform::rewrite(&mut function, &mut self.diagnostics);
            members.push(ClassMember {
                key,
                kind,
                is_static,
                computed,
                function: Box::new(function),
                span: self.span_from(member_start),
            });
        }
        self.expect(Punctuator::CloseBrace, Goal::Div);
        Class { name, heritage, members, span: self.span_from(start) }
    }

    /// Which absent class feature the member about to be parsed uses, if any.
    ///
    /// A FIELD IS TOLD FROM A METHOD BY WHAT FOLLOWS THE NAME, not by the name -- `x = 1` and
    /// `x;` are fields and `x() {}` is a method, so the lookahead has to reach past the key.
    fn unsupported_class_member(&self) -> Option<&'static str> {
        if matches!(&self.token.kind, TokenKind::PrivateName(_)) {
            return Some("private class members");
        }
        if self.at_keyword("async") && !self.next_is_member_terminator() {
            return Some("async methods");
        }
        if self.at(Punctuator::OpenBrace) {
            return Some("class static blocks");
        }
        None
    }

    /// Steps over a refused class member, so one absence produces one diagnostic.
    ///
    /// **A MEMBER ENDS AT A `;` OR AFTER ITS BALANCED BODY, and stopping at the parameter list
    /// leaves the body behind.** The first version returned when the `(...)` closed, so `*g() {}`
    /// refused the generator and then met `{ }` as a fresh member -- reporting a static block that
    /// nobody wrote. One absence, two diagnostics, and the second one names a feature the source
    /// does not contain.
    fn skip_class_member(&mut self) {
        let mut depth = 0usize;
        loop {
            match &self.token.kind {
                TokenKind::EndOfFile => return,
                TokenKind::Punctuator(
                    Punctuator::OpenBrace | Punctuator::OpenParen | Punctuator::OpenBracket,
                ) => depth += 1,
                TokenKind::Punctuator(Punctuator::CloseBrace) if depth == 0 => return,
                TokenKind::Punctuator(
                    Punctuator::CloseBrace | Punctuator::CloseParen | Punctuator::CloseBracket,
                ) => {
                    depth -= 1;
                    if depth == 0
                        && matches!(
                            self.token.kind,
                            TokenKind::Punctuator(Punctuator::CloseBrace)
                        )
                    {
                        self.advance(Goal::Div);
                        return;
                    }
                }
                TokenKind::Punctuator(Punctuator::Semicolon) if depth == 0 => {
                    self.advance(Goal::RegExp);
                    return;
                }
                _ => {}
            }
            self.advance(Goal::Div);
        }
    }

    /// Whether the token after `static` shows that `static` was itself the member's NAME.
    ///
    /// `static` IS CONTEXTUAL: `static m() {}` is a static method and `static() {}` is a method
    /// CALLED `static`. Only the next token separates them, exactly as with `get` and `set`.
    fn next_is_member_terminator(&self) -> bool {
        let mut lookahead = self.lexer;
        let next = lookahead.next_token(Goal::Div);
        matches!(
            next.map(|token| token.kind),
            Ok(TokenKind::Punctuator(Punctuator::OpenParen))
                | Ok(TokenKind::Punctuator(Punctuator::Equals))
                | Ok(TokenKind::Punctuator(Punctuator::Semicolon))
                | Ok(TokenKind::Punctuator(Punctuator::CloseBrace))
        )
    }

    /// Consumes a `{ ... }` region, counting nesting. Used only to step over refused surface.
    fn skip_balanced_braces(&mut self) {
        if !self.at(Punctuator::OpenBrace) {
            return;
        }
        let mut depth = 0usize;
        loop {
            if self.at_end() {
                return;
            }
            if self.at(Punctuator::OpenBrace) {
                depth += 1;
            } else if self.at(Punctuator::CloseBrace) {
                depth -= 1;
                if depth == 0 {
                    self.advance(Goal::Div);
                    return;
                }
            }
            self.advance(Goal::RegExp);
        }
    }

    /// `with (expr) statement`.
    ///
    /// Sloppy-mode core language, and a strict-mode EARLY ERROR.
    ///
    /// **A STATEMENT WITH NO ARM IS NOT A REFUSAL, IT IS A DIFFERENT PARSE.** With no `with` arm
    /// the keyword falls through to an expression statement and `with (a) { b(); }` reads as the
    /// identifier `with`, a call, and a separate block -- three statements that happen not to
    /// error, which a corpus scores as ACCEPTED. Parsing it into a Block and running the body with
    /// no dynamic scope is the second wrong answer of the same shape: `with ({x: 1}) { x; }`
    /// reports `x is not defined`.
    ///
    /// THE STRICT CASE IS AN EARLY ERROR AND IS NOT AN ABSENCE. The standard makes `with` a
    /// SyntaxError there, and the corpus has negative tests naming exactly that -- so reporting it
    /// as out-of-profile would turn a rule this engine DOES implement into a gap it does not have.
    ///
    /// The second early error -- `with (o) lbl: function f() {}` -- is not written here because
    /// [`Self::parse_nested_statement`] owns it for every position that admits a *Statement*. One
    /// home rather than a copy per caller.
    fn parse_with(&mut self, context: Context, start: usize) -> Statement {
        if context.strict {
            let span = self.token.span;
            self.early_error(span, "`with` is a SyntaxError in strict mode");
        }
        self.advance(Goal::RegExp);
        self.expect(Punctuator::OpenParen, Goal::RegExp);
        let object = self.parse_expression(context.with_in());
        self.expect(Punctuator::CloseParen, Goal::RegExp);
        let body = Box::new(self.parse_nested_statement(context));
        Statement::With { object, body, span: self.span_from(start) }
    }

    fn parse_if(&mut self, context: Context, start: usize) -> Statement {
        self.advance(Goal::RegExp);
        self.expect(Punctuator::OpenParen, Goal::RegExp);
        let test = self.parse_expression(context.with_in());
        self.expect(Punctuator::CloseParen, Goal::RegExp);
        let consequent = Box::new(self.parse_nested_statement(context));
        let alternate = if self.eat_keyword("else", Goal::RegExp) {
            Some(Box::new(self.parse_nested_statement(context)))
        } else {
            None
        };
        Statement::If { test, consequent, alternate, span: self.span_from(start) }
    }

    fn parse_while(&mut self, context: Context, start: usize) -> Statement {
        self.advance(Goal::RegExp);
        self.expect(Punctuator::OpenParen, Goal::RegExp);
        let test = self.parse_expression(context.with_in());
        self.expect(Punctuator::CloseParen, Goal::RegExp);
        let body = Box::new(self.parse_nested_statement(context));
        Statement::While { test, body, span: self.span_from(start) }
    }

    fn parse_do_while(&mut self, context: Context, start: usize) -> Statement {
        self.advance(Goal::RegExp);
        let body = Box::new(self.parse_nested_statement(context));
        if !self.eat_keyword("while", Goal::RegExp) {
            self.error(DiagnosticKind::MissingToken, "expected `while`");
        }
        self.expect(Punctuator::OpenParen, Goal::RegExp);
        let test = self.parse_expression(context.with_in());
        self.expect(Punctuator::CloseParen, Goal::Div);
        self.eat(Punctuator::Semicolon, Goal::RegExp);
        Statement::DoWhile { body, test, span: self.span_from(start) }
    }

    /// `for`, `for-in` and `for-of`, which share a head that cannot be told apart until after it.
    ///
    /// **The init clause is parsed with `[In]` WITHDRAWN.** Without that,
    /// `for (var i = 'a' in b; ;)` parses as a for-in loop over `b` -- the parameter people forget,
    /// and the reason it exists as a parameter at all.
    fn parse_for(&mut self, context: Context, start: usize) -> Statement {
        self.advance(Goal::RegExp);
        self.expect(Punctuator::OpenParen, Goal::RegExp);
        let no_in = context.without_in();

        if self.at(Punctuator::Semicolon) {
            self.advance(Goal::RegExp);
            return self.finish_c_style_for(context, None, start);
        }

        let init_start = self.token.span.start;
        let mut init = if let Some(kind) = self.declaration_kind_here() {
            self.advance(Goal::RegExp);
            let declarations = self.parse_declarator_list(no_in, kind, false);
            ForInit::Declaration { kind, declarations, span: self.span_from(init_start) }
        } else {
            ForInit::Expression(self.parse_expression(no_in))
        };

        let head_is_iterating = self.at_keyword("of") || self.at_keyword("in");
        if head_is_iterating {
            if let ForInit::Expression(expression) = &init {
                let expression = expression.clone();
                if let AssignmentTarget::Pattern { pattern, .. } =
                    self.refine_to_assignment_target(expression, context)
                {
                    init = ForInit::Pattern(pattern);
                }
            }
        }

        if self.at_keyword("of") {
            self.advance(Goal::RegExp);
            let right = self.parse_assignment(context.with_in());
            self.expect(Punctuator::CloseParen, Goal::RegExp);
            let body = Box::new(self.parse_nested_statement(context));
            return Statement::ForOf {
                left: Box::new(init),
                right,
                body,
                span: self.span_from(start),
            };
        }
        if self.at_keyword("in") {
            self.advance(Goal::RegExp);
            let right = self.parse_expression(context.with_in());
            self.expect(Punctuator::CloseParen, Goal::RegExp);
            let body = Box::new(self.parse_nested_statement(context));
            return Statement::ForIn {
                left: Box::new(init),
                right,
                body,
                span: self.span_from(start),
            };
        }

        self.expect(Punctuator::Semicolon, Goal::RegExp);
        self.finish_c_style_for(context, Some(Box::new(init)), start)
    }

    fn declaration_kind_here(&self) -> Option<DeclarationKind> {
        match self.token.keyword_text()? {
            "var" => Some(DeclarationKind::Var),
            "const" => Some(DeclarationKind::Const),
            "let" if self.let_begins_a_declaration() => Some(DeclarationKind::Let),
            _ => None,
        }
    }

    fn finish_c_style_for(
        &mut self,
        context: Context,
        init: Option<Box<ForInit>>,
        start: usize,
    ) -> Statement {
        if let Some(init) = init.as_deref() {
            if let ForInit::Declaration { kind: DeclarationKind::Const, declarations, .. } = init {
                for declarator in declarations {
                    if declarator.init.is_none() {
                        let span = declarator.span;
                        self.early_error(span, "a `const` declaration must have an initializer");
                    }
                }
            }
        }
        let test = if self.at(Punctuator::Semicolon) {
            None
        } else {
            Some(self.parse_expression(context.with_in()))
        };
        self.expect(Punctuator::Semicolon, Goal::RegExp);
        let update = if self.at(Punctuator::CloseParen) {
            None
        } else {
            Some(self.parse_expression(context.with_in()))
        };
        self.expect(Punctuator::CloseParen, Goal::RegExp);
        let body = Box::new(self.parse_nested_statement(context));
        Statement::For { init, test, update, body, span: self.span_from(start) }
    }

    /// `return`, and the restricted production that makes `return \n x` return undefined.
    fn parse_return(&mut self, context: Context, start: usize) -> Statement {
        if !context.allow_return {
            let span = self.token.span;
            self.early_error(span, "`return` outside a function");
        }
        self.advance(Goal::RegExp);
        let argument = if self.at(Punctuator::Semicolon)
            || self.at(Punctuator::CloseBrace)
            || self.at_end()
            || self.token.preceded_by_line_terminator
        {
            None
        } else {
            Some(self.parse_expression(context.with_in()))
        };
        self.consume_semicolon();
        Statement::Return { argument, span: self.span_from(start) }
    }

    fn parse_break_or_continue(&mut self, is_break: bool, start: usize) -> Statement {
        self.advance(Goal::RegExp);
        let label = if self.token.preceded_by_line_terminator {
            None
        } else if let TokenKind::Identifier(name) = self.token.kind.clone() {
            self.advance(Goal::Div);
            Some(name)
        } else {
            None
        };
        self.consume_semicolon();
        let span = self.span_from(start);
        if is_break {
            Statement::Break { label, span }
        } else {
            Statement::Continue { label, span }
        }
    }

    fn parse_throw(&mut self, context: Context, start: usize) -> Statement {
        self.advance(Goal::RegExp);
        if self.token.preceded_by_line_terminator {
            self.error(DiagnosticKind::UnexpectedToken, "no line break allowed after `throw`");
        }
        let argument = self.parse_expression(context.with_in());
        self.consume_semicolon();
        Statement::Throw { argument, span: self.span_from(start) }
    }

    fn parse_try(&mut self, context: Context, start: usize) -> Statement {
        self.advance(Goal::RegExp);
        self.expect(Punctuator::OpenBrace, Goal::RegExp);
        let block = self.parse_statement_list(context, Punctuator::CloseBrace);
        self.expect(Punctuator::CloseBrace, Goal::RegExp);

        let handler = if self.at_keyword("catch") {
            let catch_start = self.token.span.start;
            self.advance(Goal::RegExp);
            let param = if self.eat(Punctuator::OpenParen, Goal::RegExp) {
                let pattern = self.parse_binding_target(context);
                self.expect(Punctuator::CloseParen, Goal::RegExp);
                Some(pattern)
            } else {
                None
            };
            self.expect(Punctuator::OpenBrace, Goal::RegExp);
            let body = self.parse_statement_list(context, Punctuator::CloseBrace);
            self.expect(Punctuator::CloseBrace, Goal::RegExp);
            Some(CatchClause { param, body, span: self.span_from(catch_start) })
        } else {
            None
        };

        let finalizer = if self.eat_keyword("finally", Goal::RegExp) {
            self.expect(Punctuator::OpenBrace, Goal::RegExp);
            let body = self.parse_statement_list(context, Punctuator::CloseBrace);
            self.expect(Punctuator::CloseBrace, Goal::RegExp);
            Some(body)
        } else {
            None
        };

        if handler.is_none() && finalizer.is_none() {
            let span = self.span_from(start);
            self.early_error(span, "a `try` needs a `catch` or a `finally`");
        }
        Statement::Try { block, handler, finalizer, span: self.span_from(start) }
    }

    fn parse_switch(&mut self, context: Context, start: usize) -> Statement {
        self.advance(Goal::RegExp);
        self.expect(Punctuator::OpenParen, Goal::RegExp);
        let discriminant = self.parse_expression(context.with_in());
        self.expect(Punctuator::CloseParen, Goal::RegExp);
        self.expect(Punctuator::OpenBrace, Goal::RegExp);

        let mut cases = Vec::new();
        let mut seen_default = false;
        while !self.at_end() && !self.at(Punctuator::CloseBrace) {
            let case_start = self.token.span.start;
            let test = if self.eat_keyword("case", Goal::RegExp) {
                Some(self.parse_expression(context.with_in()))
            } else if self.at_keyword("default") {
                if seen_default {
                    let span = self.token.span;
                    self.early_error(span, "a `switch` may have only one `default` clause");
                }
                seen_default = true;
                self.advance(Goal::RegExp);
                None
            } else {
                self.error(DiagnosticKind::UnexpectedToken, "expected `case` or `default`");
                self.advance(Goal::RegExp);
                continue;
            };
            self.expect(Punctuator::Colon, Goal::RegExp);
            let mut body = Vec::new();
            while !self.at_end()
                && !self.at(Punctuator::CloseBrace)
                && !self.at_keyword("case")
                && !self.at_keyword("default")
            {
                let before = self.token_start;
                body.push(self.parse_statement(context));
                if self.token_start == before {
                    self.advance(Goal::RegExp);
                }
            }
            cases.push(SwitchCase { test, body, span: self.span_from(case_start) });
        }
        self.expect(Punctuator::CloseBrace, Goal::Div);
        Statement::Switch { discriminant, cases, span: self.span_from(start) }
    }

    fn parse_function(&mut self, context: Context, start: usize, is_async: bool) -> Function {
        self.advance(Goal::Div);
        let is_generator = self.eat(Punctuator::Star, Goal::Div);
        let name_span = self.token.span;
        let name = if let TokenKind::Identifier(name) = self.token.kind.clone() {
            self.advance(Goal::Div);
            Some(name)
        } else {
            None
        };
        let body_context = context.function_body(is_generator, is_async);
        let params_start = self.token.span.start;
        let params = self.parse_parameter_list(body_context, false);
        let params_span = self.span_from(params_start);
        self.expect(Punctuator::OpenBrace, Goal::RegExp);
        let (body, strict, declared_strict) =
            self.parse_directive_prologue_and_body(body_context, Some(Punctuator::CloseBrace));
        self.expect(Punctuator::CloseBrace, Goal::Div);
        self.recheck_parameters_after_body(&params, params_span, strict, declared_strict);

        if let Some(name) = &name {
            if (strict || context.strict) && (name == "eval" || name == "arguments") {
                self.early_error(
                    name_span,
                    format!("a strict function may not be named `{name}`"),
                );
            }
            let effective = Context { strict: strict || context.strict, ..context };
            if reserved_in(name, effective) {
                self.early_error(
                    name_span,
                    format!("`{name}` is a reserved word and may not name a function"),
                );
            }
        }
        let mut function =
            Function { name, params, body, is_generator, is_async, strict, span: self.span_from(start) };
        crate::generator_transform::rewrite(&mut function, &mut self.diagnostics);
        function
    }

    /// Re-judges a parameter list once the body's directive prologue has decided the mode.
    ///
    /// # WARNING: THE PARAMETERS ARE PARSED BEFORE THE MODE IS KNOWN
    ///
    /// `function f(a, a) { 'use strict'; }` is a SyntaxError and `function f(a, a) {}` is not. The
    /// parameters are read first, so nothing at that point can tell which this is -- the same
    /// deferral the function NAME needs, and the for-head, and the cover grammar.
    ///
    /// Three rules ride on it:
    ///
    /// - duplicate parameters, forbidden once strict;
    /// - `eval` and `arguments` as parameter names, forbidden once strict;
    /// - and the famous one: **a `"use strict"` directive is itself an early error when the
    ///   parameter list is NON-SIMPLE.** `function f(a = 1) { 'use strict'; }` is invalid, because
    ///   the defaults would have to be evaluated in a mode the directive has not established yet.
    ///   It is only an error for a directive the BODY supplies -- inheriting strictness from an
    ///   enclosing scope is fine, so `'use strict'; function f(a = 1) {}` is perfectly legal.
    fn recheck_parameters_after_body(
        &mut self,
        params: &[Pattern],
        span: Span,
        effective_strict: bool,
        declared_strict: bool,
    ) {
        let simple = params.iter().all(|p| matches!(p, Pattern::Identifier { .. }));

        if declared_strict && !simple {
            self.early_error(
                span,
                "a `use strict` directive is not allowed in a function with a non-simple parameter list",
            );
        }
        if !effective_strict {
            return;
        }
        let mut names: Vec<(String, Span)> = Vec::new();
        for param in params {
            collect_bound_names(param, &mut |name, span| names.push((name.to_string(), span)));
        }
        let mut seen: Vec<String> = Vec::new();
        for (name, name_span) in names {
            if name == "eval" || name == "arguments" {
                self.early_error(
                    name_span,
                    format!("`{name}` may not be a parameter name in strict mode"),
                );
            }
            if seen.contains(&name) {
                self.early_error(name_span, format!("duplicate parameter name `{name}`"));
            }
            seen.push(name);
        }
    }

    /// Parses a parameter list.
    ///
    /// `always_unique` is true for arrows, methods and accessors, where duplicate names are an early
    /// error **in every mode** -- only a plain function with a simple list may repeat one, and only
    /// in sloppy code. The mode alone does not decide it, which is what the first version got wrong.
    fn parse_parameter_list(&mut self, context: Context, always_unique: bool) -> Vec<Pattern> {
        let mut params = Vec::new();
        self.expect(Punctuator::OpenParen, Goal::RegExp);
        while !self.at_end() && !self.at(Punctuator::CloseParen) {
            let start = self.token.span.start;
            if self.eat(Punctuator::Ellipsis, Goal::RegExp) {
                let argument = self.parse_binding_target(context);
                params.push(Pattern::Rest {
                    argument: Box::new(argument),
                    span: self.span_from(start),
                });
                break;
            }
            let target = self.parse_binding_target(context);
            let param = if self.eat(Punctuator::Equals, Goal::RegExp) {
                let value = self.parse_assignment(context.with_in());
                Pattern::Default {
                    target: Box::new(target),
                    value: Box::new(value),
                    span: self.span_from(start),
                }
            } else {
                target
            };
            params.push(param);
            if !self.eat(Punctuator::Comma, Goal::RegExp) {
                break;
            }
        }
        self.expect(Punctuator::CloseParen, Goal::Div);
        self.check_duplicate_parameters(&params, context, always_unique);
        params
    }

    /// Duplicate formal parameters.
    ///
    /// **Legal in a sloppy function with a SIMPLE parameter list, and an early error everywhere
    /// else** -- in strict mode, and whenever any parameter is a default, a rest or a pattern. So
    /// the check is conditional on both the mode and the SHAPE of the list, and a version that
    /// always rejects would break a great deal of legacy sloppy code.
    fn check_duplicate_parameters(
        &mut self,
        params: &[Pattern],
        context: Context,
        always_unique: bool,
    ) {
        let simple = params.iter().all(|p| matches!(p, Pattern::Identifier { .. }));
        if !always_unique && !context.strict && simple {
            return;
        }
        let mut seen: Vec<&str> = Vec::new();
        let mut duplicates: Vec<(String, Span)> = Vec::new();
        for param in params {
            collect_bound_names(param, &mut |name, span| {
                if seen.contains(&name) {
                    duplicates.push((name.to_string(), span));
                } else {
                    seen.push(name);
                }
            });
        }
        for (name, span) in duplicates {
            self.early_error(span, format!("duplicate parameter name `{name}`"));
        }
    }


    /// The comma operator, which is the lowest-precedence expression form.
    pub fn parse_expression(&mut self, context: Context) -> Expression {
        let start = self.token.span.start;
        let first = self.parse_assignment(context);
        if !self.at(Punctuator::Comma) {
            return first;
        }
        let mut expressions = vec![first];
        while self.eat(Punctuator::Comma, Goal::RegExp) {
            expressions.push(self.parse_assignment(context));
        }
        Expression::Sequence { expressions, span: self.span_from(start) }
    }

    /// Assignment, which is where the cover grammar is resolved.
    ///
    /// The left side is parsed as an ordinary expression and **refined into a pattern** if an
    /// assignment operator follows -- that is the whole cover-grammar mechanism, and it is why
    /// `[a, {b}] = c` can be an array literal right up until the `=` arrives.
    pub fn parse_assignment(&mut self, context: Context) -> Expression {
        let start = self.token.span.start;

        if context.allow_yield && self.at_keyword("yield") {
            return self.parse_yield(context, start);
        }

        if let Some(arrow) = self.try_parse_arrow(context) {
            return arrow;
        }

        let left = self.parse_conditional(context);
        let Some(operator) = self.assignment_operator_here() else {
            return left;
        };
        self.advance(Goal::RegExp);
        let value = self.parse_assignment(context);
        let target = if operator == AssignmentOperator::Assign {
            Box::new(self.refine_to_assignment_target(left, context))
        } else {
            Box::new(self.refine_to_simple_target(left, context))
        };
        Expression::Assignment { operator, target, value: Box::new(value), span: self.span_from(start) }
    }

    fn parse_yield(&mut self, context: Context, start: usize) -> Expression {
        self.advance(Goal::RegExp);
        let delegate = self.eat(Punctuator::Star, Goal::RegExp);
        let argument = if self.at(Punctuator::CloseParen)
            || self.at(Punctuator::CloseBracket)
            || self.at(Punctuator::CloseBrace)
            || self.at(Punctuator::Comma)
            || self.at(Punctuator::Semicolon)
            || self.at_end()
            || self.token.preceded_by_line_terminator
        {
            None
        } else {
            Some(self.parse_assignment(context))
        };
        Expression::Yield {
            argument: argument.map(Box::new),
            delegate,
            span: self.span_from(start),
        }
    }

    fn assignment_operator_here(&self) -> Option<AssignmentOperator> {
        use AssignmentOperator as A;
        let TokenKind::Punctuator(punctuator) = self.token.kind else { return None };
        Some(match punctuator {
            Punctuator::Equals => A::Assign,
            Punctuator::PlusEquals => A::AddAssign,
            Punctuator::MinusEquals => A::SubtractAssign,
            Punctuator::StarEquals => A::MultiplyAssign,
            Punctuator::SlashEquals => A::DivideAssign,
            Punctuator::PercentEquals => A::RemainderAssign,
            Punctuator::StarStarEquals => A::ExponentAssign,
            Punctuator::LessThanLessThanEquals => A::ShiftLeftAssign,
            Punctuator::GreaterThanGreaterThanEquals => A::ShiftRightAssign,
            Punctuator::GreaterThanGreaterThanGreaterThanEquals => A::UnsignedShiftRightAssign,
            Punctuator::AmpersandEquals => A::BitwiseAndAssign,
            Punctuator::BarEquals => A::BitwiseOrAssign,
            Punctuator::CaretEquals => A::BitwiseXorAssign,
            Punctuator::AmpersandAmpersandEquals => A::LogicalAndAssign,
            Punctuator::BarBarEquals => A::LogicalOrAssign,
            Punctuator::QuestionQuestionEquals => A::NullishAssign,
            _ => return None,
        })
    }

    fn parse_conditional(&mut self, context: Context) -> Expression {
        let start = self.token.span.start;
        let test = self.parse_nullish(context);
        if !self.eat(Punctuator::Question, Goal::RegExp) {
            return test;
        }
        let consequent = self.parse_assignment(context.with_in());
        self.expect(Punctuator::Colon, Goal::RegExp);
        let alternate = self.parse_assignment(context);
        Expression::Conditional {
            test: Box::new(test),
            consequent: Box::new(consequent),
            alternate: Box::new(alternate),
            span: self.span_from(start),
        }
    }

    /// `??`, and the grammar rule that it may not mix with `&&`/`||` unparenthesized.
    fn parse_nullish(&mut self, context: Context) -> Expression {
        let start = self.token.span.start;
        let mut left = self.parse_logical_or(context);
        while self.at(Punctuator::QuestionQuestion) {
            if matches!(
                left,
                Expression::Logical {
                    operator: LogicalOperator::And | LogicalOperator::Or,
                    ..
                }
            ) {
                let span = self.token.span;
                self.early_error(span, "`??` may not be mixed with `&&` or `||` without parentheses");
            }
            self.advance(Goal::RegExp);
            let right = self.parse_logical_or(context);
            if matches!(
                right,
                Expression::Logical {
                    operator: LogicalOperator::And | LogicalOperator::Or,
                    ..
                }
            ) {
                let span = right.span();
                self.early_error(span, "`??` may not be mixed with `&&` or `||` without parentheses");
            }
            left = Expression::Logical {
                operator: LogicalOperator::NullishCoalescing,
                left: Box::new(left),
                right: Box::new(right),
                span: self.span_from(start),
            };
        }
        left
    }

    fn parse_logical_or(&mut self, context: Context) -> Expression {
        let start = self.token.span.start;
        let mut left = self.parse_logical_and(context);
        while self.at(Punctuator::BarBar) {
            self.advance(Goal::RegExp);
            let right = self.parse_logical_and(context);
            left = Expression::Logical {
                operator: LogicalOperator::Or,
                left: Box::new(left),
                right: Box::new(right),
                span: self.span_from(start),
            };
        }
        left
    }

    fn parse_logical_and(&mut self, context: Context) -> Expression {
        let start = self.token.span.start;
        let mut left = self.parse_binary(context, 0);
        while self.at(Punctuator::AmpersandAmpersand) {
            self.advance(Goal::RegExp);
            let right = self.parse_binary(context, 0);
            left = Expression::Logical {
                operator: LogicalOperator::And,
                left: Box::new(left),
                right: Box::new(right),
                span: self.span_from(start),
            };
        }
        left
    }

    /// The binary ladder, by precedence climbing.
    ///
    /// One function rather than nine, because nine near-identical functions are nine places for a
    /// precedence to be written down wrongly. The table is the single source of the ordering.
    fn parse_binary(&mut self, context: Context, min_precedence: u8) -> Expression {
        let start = self.token.span.start;
        let mut left = self.parse_unary(context);
        loop {
            let Some((operator, precedence, right_associative)) = self.binary_operator_here(context)
            else {
                break;
            };
            if precedence < min_precedence {
                break;
            }
            self.advance(Goal::RegExp);
            let next_min = if right_associative { precedence } else { precedence + 1 };
            let right = self.parse_binary(context, next_min);
            left = Expression::Binary {
                operator,
                left: Box::new(left),
                right: Box::new(right),
                span: self.span_from(start),
            };
        }
        left
    }

    /// The operator here, its precedence, and whether it associates right.
    ///
    /// `in` is gated on `[In]` -- that single check is what keeps `for (var i = 'a' in b; ;)`
    /// from parsing as a for-in loop.
    fn binary_operator_here(&self, context: Context) -> Option<(BinaryOperator, u8, bool)> {
        use BinaryOperator as B;
        if let Some(keyword) = self.token.keyword_text() {
            return match keyword {
                "instanceof" => Some((B::InstanceOf, 7, false)),
                "in" if context.allow_in => Some((B::In, 7, false)),
                _ => None,
            };
        }
        let TokenKind::Punctuator(punctuator) = self.token.kind else { return None };
        Some(match punctuator {
            Punctuator::Bar => (B::BitwiseOr, 1, false),
            Punctuator::Caret => (B::BitwiseXor, 2, false),
            Punctuator::Ampersand => (B::BitwiseAnd, 3, false),
            Punctuator::EqualsEquals => (B::Equal, 4, false),
            Punctuator::ExclamationEquals => (B::NotEqual, 4, false),
            Punctuator::EqualsEqualsEquals => (B::StrictEqual, 4, false),
            Punctuator::ExclamationEqualsEquals => (B::StrictNotEqual, 4, false),
            Punctuator::LessThan => (B::Less, 7, false),
            Punctuator::LessThanEquals => (B::LessEqual, 7, false),
            Punctuator::GreaterThan => (B::Greater, 7, false),
            Punctuator::GreaterThanEquals => (B::GreaterEqual, 7, false),
            Punctuator::LessThanLessThan => (B::ShiftLeft, 8, false),
            Punctuator::GreaterThanGreaterThan => (B::ShiftRight, 8, false),
            Punctuator::GreaterThanGreaterThanGreaterThan => (B::UnsignedShiftRight, 8, false),
            Punctuator::Plus => (B::Add, 9, false),
            Punctuator::Minus => (B::Subtract, 9, false),
            Punctuator::Star => (B::Multiply, 10, false),
            Punctuator::Slash => (B::Divide, 10, false),
            Punctuator::Percent => (B::Remainder, 10, false),
            Punctuator::StarStar => (B::Exponent, 11, true),
            _ => return None,
        })
    }

    fn parse_unary(&mut self, context: Context) -> Expression {
        let start = self.token.span.start;

        let unary_operator = if let Some(keyword) = self.token.keyword_text() {
            match keyword {
                "typeof" => Some(UnaryOperator::TypeOf),
                "void" => Some(UnaryOperator::Void),
                "delete" => Some(UnaryOperator::Delete),
                "await" if context.allow_await => Some(UnaryOperator::Await),
                _ => None,
            }
        } else {
            match self.token.kind {
                TokenKind::Punctuator(Punctuator::Minus) => Some(UnaryOperator::Minus),
                TokenKind::Punctuator(Punctuator::Plus) => Some(UnaryOperator::Plus),
                TokenKind::Punctuator(Punctuator::Exclamation) => Some(UnaryOperator::Not),
                TokenKind::Punctuator(Punctuator::Tilde) => Some(UnaryOperator::BitwiseNot),
                _ => None,
            }
        };

        if let Some(operator) = unary_operator {
            self.advance(Goal::RegExp);
            let argument = self.parse_unary(context);
            let span = self.span_from(start);
            if operator == UnaryOperator::Delete && context.strict {
                if let (Expression::Identifier { .. }, _) = parenthesized_inner(&argument) {
                    self.early_error(span, "`delete` of an unqualified name is a SyntaxError in strict mode");
                }
            }
            if self.at(Punctuator::StarStar) {
                let star = self.token.span;
                self.early_error(star, "an unparenthesized unary expression may not be the left operand of `**`");
            }
            return Expression::Unary { operator, argument: Box::new(argument), span };
        }

        if self.at(Punctuator::PlusPlus) || self.at(Punctuator::MinusMinus) {
            let operator = if self.at(Punctuator::PlusPlus) {
                UpdateOperator::Increment
            } else {
                UpdateOperator::Decrement
            };
            self.advance(Goal::RegExp);
            let argument = self.parse_unary(context);
            self.check_update_target(&argument, context);
            return Expression::Update {
                operator,
                prefix: true,
                argument: Box::new(argument),
                span: self.span_from(start),
            };
        }

        self.parse_postfix(context, start)
    }

    fn parse_postfix(&mut self, context: Context, start: usize) -> Expression {
        let argument = self.parse_call_or_member(context);
        if (self.at(Punctuator::PlusPlus) || self.at(Punctuator::MinusMinus))
            && !self.token.preceded_by_line_terminator
        {
            let operator = if self.at(Punctuator::PlusPlus) {
                UpdateOperator::Increment
            } else {
                UpdateOperator::Decrement
            };
            self.advance(Goal::Div);
            self.check_update_target(&argument, context);
            return Expression::Update {
                operator,
                prefix: false,
                argument: Box::new(argument),
                span: self.span_from(start),
            };
        }
        argument
    }

    /// The operand of `++`/`--` must be a SIMPLE assignment target.
    ///
    /// `1++`, `f()++` and `(a + b)++` are SyntaxErrors -- an update writes back, so its operand must
    /// be something that can be written. Unlike the assignment rules, a destructuring pattern is not
    /// allowed either: `[a]++` is invalid where `[a] = b` is fine.
    ///
    /// Parentheses stay transparent here, so `(a)++` is legal -- the rule is about the inner
    /// expression, the same shape as the assignment-target rule.
    fn check_update_target(&mut self, argument: &Expression, context: Context) {
        let (inner, _) = parenthesized_inner(argument);
        match inner {
            Expression::Identifier { name, .. } => {
                if context.strict && (name == "eval" || name == "arguments") {
                    let span = inner.span();
                    let name = name.clone();
                    self.early_error(span, format!("`{name}` may not be updated in strict mode"));
                }
            }
            Expression::Member { object, optional, .. } => {
                if *optional || chain_is_optional(object) {
                    let span = inner.span();
                    self.early_error(span, "an optional chain is not an update target");
                }
            }
            _ => {
                let span = inner.span();
                self.early_error(span, "the operand of `++`/`--` must be a simple assignment target");
            }
        }
    }

    fn parse_call_or_member(&mut self, context: Context) -> Expression {
        let start = self.token.span.start;
        let mut expression = if self.at_keyword("new") {
            self.parse_new(context, start)
        } else {
            self.parse_primary(context)
        };

        loop {
            if self.eat(Punctuator::Dot, Goal::Div) {
                let property = self.parse_member_property();
                expression = Expression::Member {
                    object: Box::new(expression),
                    property: Box::new(property),
                    optional: false,
                    span: self.span_from(start),
                };
                continue;
            }
            if self.at(Punctuator::QuestionDot) {
                self.advance(Goal::Div);
                if self.at(Punctuator::OpenParen) {
                    let arguments = self.parse_arguments(context);
                    expression = Expression::Call {
                        callee: Box::new(expression),
                        arguments,
                        optional: true,
                        span: self.span_from(start),
                    };
                } else if self.eat(Punctuator::OpenBracket, Goal::RegExp) {
                    let index = self.parse_expression(context.with_in());
                    let span = self.span_from(start);
                    self.expect(Punctuator::CloseBracket, Goal::Div);
                    expression = Expression::Member {
                        object: Box::new(expression),
                        property: Box::new(MemberProperty::Computed { expression: index, span }),
                        optional: true,
                        span: self.span_from(start),
                    };
                } else {
                    let property = self.parse_member_property();
                    expression = Expression::Member {
                        object: Box::new(expression),
                        property: Box::new(property),
                        optional: true,
                        span: self.span_from(start),
                    };
                }
                continue;
            }
            if self.eat(Punctuator::OpenBracket, Goal::RegExp) {
                let index = self.parse_expression(context.with_in());
                let span = self.span_from(start);
                self.expect(Punctuator::CloseBracket, Goal::Div);
                expression = Expression::Member {
                    object: Box::new(expression),
                    property: Box::new(MemberProperty::Computed { expression: index, span }),
                    optional: false,
                    span: self.span_from(start),
                };
                continue;
            }
            if self.at(Punctuator::OpenParen) {
                let arguments = self.parse_arguments(context);
                expression = Expression::Call {
                    callee: Box::new(expression),
                    arguments,
                    optional: false,
                    span: self.span_from(start),
                };
                continue;
            }
            if matches!(self.token.kind, TokenKind::Template(_)) {
                if chain_is_optional(&expression) {
                    let span = self.token.span;
                    self.early_error(span, "a template may not be tagged by an optional chain");
                }
                let quasi = self.parse_template(context.tagged(true));
                expression = Expression::Tagged {
                    tag: Box::new(expression),
                    quasi: Box::new(quasi),
                    span: self.span_from(start),
                };
                continue;
            }
            return expression;
        }
    }

    fn parse_member_property(&mut self) -> MemberProperty {
        let start = self.token.span.start;
        match self.token.kind.clone() {
            TokenKind::Identifier(name) => {
                self.advance(Goal::Div);
                MemberProperty::Identifier { name, span: self.span_from(start) }
            }
            TokenKind::PrivateName(name) => {
                self.advance(Goal::Div);
                MemberProperty::Private { name, span: self.span_from(start) }
            }
            _ => {
                self.error(DiagnosticKind::UnexpectedToken, "expected a property name");
                MemberProperty::Identifier { name: String::new(), span: self.span_from(start) }
            }
        }
    }

    fn parse_new(&mut self, context: Context, start: usize) -> Expression {
        self.advance(Goal::Div);
        if self.at(Punctuator::Dot) {
            self.advance(Goal::Div);
            let name_span = self.token.span;
            let is_target = !self.token.had_escape
                && matches!(&self.token.kind, TokenKind::Identifier(name) if name == "target");
            self.parse_member_property();
            if !is_target {
                self.early_error(name_span, "`new.` may only be followed by `target`");
            }
            return Expression::NewTarget { span: self.span_from(start) };
        }
        let callee = if self.at_keyword("new") {
            self.parse_new(context, self.token.span.start)
        } else {
            self.parse_primary(context)
        };
        let mut callee = callee;
        loop {
            if self.eat(Punctuator::Dot, Goal::Div) {
                let property = self.parse_member_property();
                callee = Expression::Member {
                    object: Box::new(callee),
                    property: Box::new(property),
                    optional: false,
                    span: self.span_from(start),
                };
                continue;
            }
            if self.eat(Punctuator::OpenBracket, Goal::RegExp) {
                let index = self.parse_expression(context.with_in());
                let span = self.span_from(start);
                self.expect(Punctuator::CloseBracket, Goal::Div);
                callee = Expression::Member {
                    object: Box::new(callee),
                    property: Box::new(MemberProperty::Computed { expression: index, span }),
                    optional: false,
                    span: self.span_from(start),
                };
                continue;
            }
            if matches!(self.token.kind, TokenKind::Template(_)) {
                if chain_is_optional(&callee) {
                    let span = self.token.span;
                    self.early_error(span, "a template may not be tagged by an optional chain");
                }
                let quasi = self.parse_template(context.tagged(true));
                callee = Expression::Tagged {
                    tag: Box::new(callee),
                    quasi: Box::new(quasi),
                    span: self.span_from(start),
                };
                continue;
            }
            break;
        }
        if self.at(Punctuator::QuestionDot) {
            let span = self.token.span;
            self.early_error(span, "`new` may not be applied to an optional chain");
        }
        let arguments =
            if self.at(Punctuator::OpenParen) { self.parse_arguments(context) } else { Vec::new() };
        Expression::New { callee: Box::new(callee), arguments, span: self.span_from(start) }
    }

    fn parse_arguments(&mut self, context: Context) -> Vec<Argument> {
        let mut arguments = Vec::new();
        self.expect(Punctuator::OpenParen, Goal::RegExp);
        while !self.at_end() && !self.at(Punctuator::CloseParen) {
            let start = self.token.span.start;
            if self.eat(Punctuator::Ellipsis, Goal::RegExp) {
                let argument = self.parse_assignment(context.with_in());
                arguments.push(Argument::Spread { argument, span: self.span_from(start) });
            } else {
                arguments.push(Argument::Expression(self.parse_assignment(context.with_in())));
            }
            if !self.eat(Punctuator::Comma, Goal::RegExp) {
                break;
            }
        }
        self.expect(Punctuator::CloseParen, Goal::Div);
        arguments
    }

    fn parse_primary(&mut self, context: Context) -> Expression {
        let start = self.token.span.start;
        match self.token.kind.clone() {
            TokenKind::Number(value) => {
                self.advance(Goal::Div);
                Expression::Number { value, span: self.span_from(start) }
            }
            TokenKind::String(value) => {
                self.advance(Goal::Div);
                Expression::String { value, span: self.span_from(start) }
            }
            TokenKind::Template(_) => self.parse_template(context.tagged(false)),
            TokenKind::RegExp { body, flags } => {
                self.advance(Goal::Div);
                let span = self.span_from(start);
                self.check_regexp_literal(&body, &flags, span);
                Expression::RegExp { body, flags, span }
            }
            TokenKind::PrivateName(name) => {
                self.advance(Goal::Div);
                Expression::Identifier { name: format!("#{name}"), span: self.span_from(start) }
            }
            TokenKind::Identifier(name) => match name.as_str() {
                "true" | "false" if !self.token.had_escape => {
                    let value = name == "true";
                    self.advance(Goal::Div);
                    Expression::Boolean { value, span: self.span_from(start) }
                }
                "null" if !self.token.had_escape => {
                    self.advance(Goal::Div);
                    Expression::Null { span: self.span_from(start) }
                }
                "this" if !self.token.had_escape => {
                    self.advance(Goal::Div);
                    Expression::This { span: self.span_from(start) }
                }
                "function" if !self.token.had_escape => {
                    let function = self.parse_function(context, start, false);
                    Expression::Function(Box::new(function))
                }
                "class" if !self.token.had_escape => {
                    Expression::Class(Box::new(self.parse_class(context)))
                }
                "super" if !self.token.had_escape => {
                    self.advance(Goal::Div);
                    Expression::Super { span: self.span_from(start) }
                }
                "async" if !self.token.had_escape && self.async_function_follows() => {
                    self.advance(Goal::Div);
                    let function = self.parse_function(context, start, true);
                    Expression::Function(Box::new(function))
                }
                _ => {
                    if reserved_in(&name, context) {
                        let span = self.token.span;
                        self.early_error(
                            span,
                            format!("`{name}` is a reserved word and may not be used as an identifier"),
                        );
                    }
                    self.advance(Goal::Div);
                    Expression::Identifier { name, span: self.span_from(start) }
                }
            },
            TokenKind::Punctuator(Punctuator::OpenBracket) => self.parse_array_literal(context),
            TokenKind::Punctuator(Punctuator::OpenBrace) => self.parse_object_literal(context),
            TokenKind::Punctuator(Punctuator::OpenParen) => {
                self.advance(Goal::RegExp);
                let inner = self.parse_expression(context.with_in());
                self.expect(Punctuator::CloseParen, Goal::Div);
                Expression::Parenthesized {
                    expression: Box::new(inner),
                    span: self.span_from(start),
                }
            }
            TokenKind::BigInt(_) => {
                self.error(DiagnosticKind::NotInProfile, "BigInt is not in this profile");
                self.advance(Goal::Div);
                Expression::Null { span: self.span_from(start) }
            }
            _ => {
                self.error(DiagnosticKind::UnexpectedToken, "expected an expression");
                Expression::Null { span: self.span_from(start) }
            }
        }
    }

    /// `async [no LineTerminator here] function` -- the newline matters here too.
    fn async_function_follows(&self) -> bool {
        let mut probe = self.lexer;
        probe.reset_to(self.token.span.end);
        match probe.next_token(Goal::Div) {
            Ok(next) => !next.preceded_by_line_terminator && next.keyword_text() == Some("function"),
            Err(_) => false,
        }
    }

    fn parse_template(&mut self, context: Context) -> Expression {
        let start = self.token.span.start;
        let mut quasis = Vec::new();
        let mut expressions = Vec::new();

        let TokenKind::Template(part) = self.token.kind.clone() else {
            self.error(DiagnosticKind::UnexpectedToken, "expected a template");
            return Expression::Null { span: self.span_from(start) };
        };
        if part.cooked.is_none() && !context.tagged {
            let span = self.token.span;
            self.early_error(span, "an invalid escape sequence is only legal in a tagged template");
        }
        let single = part.kind == TemplateKind::NoSubstitution;
        quasis.push(TemplateElement {
            cooked: part.cooked,
            raw: part.raw,
            span: self.token.span,
        });
        self.advance(Goal::RegExp);
        if single {
            return Expression::Template { quasis, expressions, span: self.span_from(start) };
        }

        loop {
            expressions.push(self.parse_expression(context.with_in()));
            if !self.at(Punctuator::CloseBrace) {
                self.error(DiagnosticKind::MissingToken, "expected `}` to close a template substitution");
                break;
            }
            self.relex(Goal::TemplateTail);
            let TokenKind::Template(part) = self.token.kind.clone() else {
                self.error(DiagnosticKind::UnexpectedToken, "expected template text");
                break;
            };
            if part.cooked.is_none() && !context.tagged {
                let span = self.token.span;
                self.early_error(span, "an invalid escape sequence is only legal in a tagged template");
            }
            let last = part.kind == TemplateKind::Tail;
            quasis.push(TemplateElement {
                cooked: part.cooked,
                raw: part.raw,
                span: self.token.span,
            });
            self.advance(Goal::Div);
            if last {
                break;
            }
        }
        Expression::Template { quasis, expressions, span: self.span_from(start) }
    }

    fn parse_array_literal(&mut self, context: Context) -> Expression {
        let start = self.token.span.start;
        self.advance(Goal::RegExp);
        let mut elements = Vec::new();
        loop {
            if self.at(Punctuator::CloseBracket) || self.at_end() {
                break;
            }
            if self.at(Punctuator::Comma) {
                elements.push(ArrayElement::Hole);
                self.advance(Goal::RegExp);
                continue;
            }
            let element_start = self.token.span.start;
            let was_spread = self.eat(Punctuator::Ellipsis, Goal::RegExp);
            let element = if was_spread {
                let argument = self.parse_assignment(context.with_in());
                Some(ArrayElement::Spread {
                    argument,
                    span: self.span_from(element_start),
                    comma_after: false,
                })
            } else {
                elements.push(ArrayElement::Expression(self.parse_assignment(context.with_in())));
                None
            };
            let comma = self.eat(Punctuator::Comma, Goal::RegExp);
            if let Some(ArrayElement::Spread { argument, span, .. }) = element {
                elements.push(ArrayElement::Spread { argument, span, comma_after: comma });
            }
            if !comma {
                break;
            }
        }
        self.expect(Punctuator::CloseBracket, Goal::Div);
        Expression::Array { elements, span: self.span_from(start) }
    }

    fn parse_object_literal(&mut self, context: Context) -> Expression {
        let start = self.token.span.start;
        self.advance(Goal::RegExp);
        let mut properties = Vec::new();
        while !self.at_end() && !self.at(Punctuator::CloseBrace) {
            let property_start = self.token.span.start;
            if self.eat(Punctuator::Ellipsis, Goal::RegExp) {
                let argument = self.parse_assignment(context.with_in());
                properties.push(ObjectProperty::Spread {
                    argument,
                    span: self.span_from(property_start),
                });
            } else if self.async_method_here() {
                self.error(DiagnosticKind::NotInProfile, "async functions are not in this profile");
                self.advance(Goal::Div);
                let _ = self.eat(Punctuator::Star, Goal::Div);
                let _ = self.parse_property_key(context);
                let body_context = context.function_body(false, true);
                let _ = self.parse_parameter_list(body_context, true);
                self.skip_balanced_braces();
            } else if self.at(Punctuator::Star) {
                self.advance(Goal::Div);
                let (key, computed) = self.parse_property_key(context);
                let body_context = context.function_body(true, false);
                let params_start = self.token.span.start;
                let params = self.parse_parameter_list(body_context, true);
                let params_span = self.span_from(params_start);
                self.expect(Punctuator::OpenBrace, Goal::RegExp);
                let (body, strict, declared_strict) = self
                    .parse_directive_prologue_and_body(body_context, Some(Punctuator::CloseBrace));
                self.expect(Punctuator::CloseBrace, Goal::Div);
                self.recheck_parameters_after_body(&params, params_span, strict, declared_strict);
                let mut function = Function {
                    name: None,
                    params,
                    body,
                    is_generator: true,
                    is_async: false,
                    strict,
                    span: self.span_from(property_start),
                };
                crate::generator_transform::rewrite(&mut function, &mut self.diagnostics);
                properties.push(ObjectProperty::Method {
                    key,
                    function: Box::new(function),
                    kind: MethodKind::Normal,
                    computed,
                    span: self.span_from(property_start),
                });
            } else if let Some(kind) = self.accessor_here() {
                self.advance(Goal::Div);
                let (key, computed) = self.parse_property_key(context);
                let body_context = context.function_body(false, false);
                let params_start = self.token.span.start;
                let params = self.parse_parameter_list(body_context, true);
                let params_span = self.span_from(params_start);
                self.expect(Punctuator::OpenBrace, Goal::RegExp);
                let (body, strict, declared_strict) =
                    self.parse_directive_prologue_and_body(body_context, Some(Punctuator::CloseBrace));
                self.expect(Punctuator::CloseBrace, Goal::Div);
                self.recheck_parameters_after_body(&params, params_span, strict, declared_strict);
                if kind == MethodKind::Set && params.len() != 1 {
                    let span = self.span_from(property_start);
                    self.early_error(span, "a setter takes exactly one parameter");
                }
                if kind == MethodKind::Get && !params.is_empty() {
                    let span = self.span_from(property_start);
                    self.early_error(span, "a getter takes no parameters");
                }
                properties.push(ObjectProperty::Method {
                    key,
                    function: Box::new(Function {
                        name: None,
                        params,
                        body,
                        is_generator: false,
                        is_async: false,
                        strict,
                        span: self.span_from(property_start),
                    }),
                    kind,
                    computed,
                    span: self.span_from(property_start),
                });
            } else {
                let (key, computed) = self.parse_property_key(context);
                if self.eat(Punctuator::Colon, Goal::RegExp) {
                    let value = self.parse_assignment(context.with_in());
                    properties.push(ObjectProperty::Property {
                        key,
                        value,
                        computed,
                        shorthand: false,
                        span: self.span_from(property_start),
                    });
                } else if self.at(Punctuator::Equals) {
                    self.advance(Goal::RegExp);
                    let value = self.parse_assignment(context.with_in());
                    let name = match &key {
                        PropertyKey::Identifier { name, .. } => name.clone(),
                        _ => String::new(),
                    };
                    properties.push(ObjectProperty::CoverInitializedName {
                        name,
                        value,
                        span: self.span_from(property_start),
                    });
                } else if self.at(Punctuator::OpenParen) {
                    let body_context = context.function_body(false, false);
                    let params_start = self.token.span.start;
                    let params = self.parse_parameter_list(body_context, true);
                    let params_span = self.span_from(params_start);
                    self.expect(Punctuator::OpenBrace, Goal::RegExp);
                    let (body, strict, declared_strict) = self
                        .parse_directive_prologue_and_body(body_context, Some(Punctuator::CloseBrace));
                    self.expect(Punctuator::CloseBrace, Goal::Div);
                    self.recheck_parameters_after_body(
                        &params,
                        params_span,
                        strict,
                        declared_strict,
                    );
                    properties.push(ObjectProperty::Method {
                        key,
                        function: Box::new(Function {
                            name: None,
                            params,
                            body,
                            is_generator: false,
                            is_async: false,
                            strict,
                            span: self.span_from(property_start),
                        }),
                        kind: MethodKind::Normal,
                        computed,
                        span: self.span_from(property_start),
                    });
                } else {
                    let (name, span) = match &key {
                        PropertyKey::Identifier { name, span } => (name.clone(), *span),
                        _ => {
                            self.error(DiagnosticKind::UnexpectedToken, "expected `:` after a property name");
                            (String::new(), self.token.span)
                        }
                    };
                    if !name.is_empty() && reserved_in(&name, context) {
                        self.early_error(
                            span,
                            format!("`{name}` is a reserved word and may not be a shorthand property"),
                        );
                    }
                    properties.push(ObjectProperty::Property {
                        key,
                        value: Expression::Identifier { name, span },
                        computed,
                        shorthand: true,
                        span: self.span_from(property_start),
                    });
                }
            }
            if !self.eat(Punctuator::Comma, Goal::RegExp) {
                break;
            }
        }
        self.expect(Punctuator::CloseBrace, Goal::Div);
        let span = self.span_from(start);

        let mut proto_definitions = 0;
        for property in &properties {
            if let ObjectProperty::Property { key, computed: false, shorthand: false, .. } = property {
                let is_proto = match key {
                    PropertyKey::Identifier { name, .. } => name == "__proto__",
                    PropertyKey::String { value, .. } => *value == JsString::from("__proto__"),
                    _ => false,
                };
                if is_proto {
                    proto_definitions += 1;
                }
            }
        }
        if proto_definitions > 1 {
            self.parked_cover_errors
                .push((span, "an object literal may define `__proto__` only once"));
        }

        for property in &properties {
            if let ObjectProperty::CoverInitializedName { span, .. } = property {
                self.parked_cover_errors
                    .push((*span, "`{ a = 1 }` is only legal as a destructuring pattern"));
            }
        }
        Expression::Object { properties, span }
    }

    /// Whether an async method definition starts here.
    ///
    /// `async` is contextual: `{async: 1}` is a property and `{async m() {}}` is a method, told
    /// apart only by what follows -- and `{async}` is a shorthand, so the check must not fire on a
    /// bare `async` either.
    fn async_method_here(&self) -> bool {
        if self.token.keyword_text() != Some("async") {
            return false;
        }
        let mut probe = self.lexer;
        probe.reset_to(self.token.span.end);
        match probe.next_token(Goal::Div) {
            Ok(next) => {
                !next.preceded_by_line_terminator
                    && (matches!(
                        next.kind,
                        TokenKind::Identifier(_) | TokenKind::String(_) | TokenKind::Number(_)
                    ) || next.is_punctuator(Punctuator::Star)
                        || next.is_punctuator(Punctuator::OpenBracket))
            }
            Err(_) => false,
        }
    }

    /// Whether an accessor definition starts here.
    ///
    /// `get`/`set` are ordinary identifiers; what makes one an accessor is that a PROPERTY NAME
    /// follows it. A `(` after it means a method merely named `get`, and a `:` means a property.
    fn accessor_here(&self) -> Option<MethodKind> {
        let kind = match self.token.keyword_text()? {
            "get" => MethodKind::Get,
            "set" => MethodKind::Set,
            _ => return None,
        };
        let mut probe = self.lexer;
        probe.reset_to(self.token.span.end);
        let next = probe.next_token(Goal::Div).ok()?;
        let begins_a_property_name = matches!(
            next.kind,
            TokenKind::Identifier(_)
                | TokenKind::String(_)
                | TokenKind::Number(_)
                | TokenKind::PrivateName(_)
        ) || next.is_punctuator(Punctuator::OpenBracket);
        begins_a_property_name.then_some(kind)
    }

    fn parse_property_key(&mut self, context: Context) -> (PropertyKey, bool) {
        let start = self.token.span.start;
        match self.token.kind.clone() {
            TokenKind::Identifier(name) => {
                self.advance(Goal::Div);
                (PropertyKey::Identifier { name, span: self.span_from(start) }, false)
            }
            TokenKind::String(value) => {
                self.advance(Goal::Div);
                (PropertyKey::String { value, span: self.span_from(start) }, false)
            }
            TokenKind::Number(value) => {
                self.advance(Goal::Div);
                (PropertyKey::Number { value, span: self.span_from(start) }, false)
            }
            TokenKind::PrivateName(name) => {
                self.advance(Goal::Div);
                (PropertyKey::Private { name, span: self.span_from(start) }, false)
            }
            TokenKind::Punctuator(Punctuator::OpenBracket) => {
                self.advance(Goal::RegExp);
                let expression = self.parse_assignment(context.with_in());
                self.expect(Punctuator::CloseBracket, Goal::Div);
                (
                    PropertyKey::Computed {
                        expression: Box::new(expression),
                        span: self.span_from(start),
                    },
                    true,
                )
            }
            _ => {
                self.error(DiagnosticKind::UnexpectedToken, "expected a property name");
                (PropertyKey::Identifier { name: String::new(), span: self.span_from(start) }, false)
            }
        }
    }


    /// Tries to parse an arrow function from here, returning `None` if this is not one.
    ///
    /// # WHY THIS IS SPECULATIVE RATHER THAN PREDICTIVE
    ///
    /// `(a, b)` is a parenthesized expression until a `=>` arrives, and no bounded lookahead settles
    /// it -- the parenthesized part can be arbitrarily long. So the parse is attempted and abandoned
    /// on failure, which costs nothing because the lexer is `Copy` and nothing was buffered.
    ///
    /// The `=>` must be on the **same line** as the parameter list: `(a) \n => a` is a
    /// SyntaxError, which is why the flag is checked rather than just the token.
    fn try_parse_arrow(&mut self, context: Context) -> Option<Expression> {
        let start = self.token.span.start;
        let saved = self.snapshot();

        let is_async = self.at_keyword("async") && !self.token.had_escape && {
            let mut probe = self.lexer;
            probe.reset_to(self.token.span.end);
            matches!(probe.next_token(Goal::Div), Ok(next)
                if !next.preceded_by_line_terminator
                    && (next.is_punctuator(Punctuator::OpenParen)
                        || matches!(next.kind, TokenKind::Identifier(_))))
        };
        let param_context = if is_async { context.function_body(false, true) } else { context };
        if is_async {
            self.advance(Goal::Div);
        }

        if let TokenKind::Identifier(name) = self.token.kind.clone() {
            let param_span = self.token.span;
            self.advance(Goal::Div);
            if self.at(Punctuator::Arrow) && !self.token.preceded_by_line_terminator {
                self.advance(Goal::RegExp);
                if reserved_in(&name, param_context) {
                    self.early_error(
                        param_span,
                        format!("`{name}` is a reserved word and may not be a parameter"),
                    );
                }
                let params = vec![Pattern::Identifier { name, span: param_span }];
                return Some(self.finish_arrow(context, params, is_async, start));
            }
            self.restore(saved, Goal::RegExp);
            return None;
        }

        if !self.at(Punctuator::OpenParen) {
            self.restore(saved, Goal::RegExp);
            return None;
        }

        let diagnostics_before = self.diagnostics.items().len();
        let parked_before = self.parked_cover_errors.len();
        let params = self.parse_parameter_list(param_context, true);
        let head_is_well_formed = self.diagnostics.items().len() == diagnostics_before;
        if !head_is_well_formed || !self.at(Punctuator::Arrow) || self.token.preceded_by_line_terminator
        {
            self.restore(saved, Goal::RegExp);
            self.diagnostics.truncate(diagnostics_before);
            self.parked_cover_errors.truncate(parked_before);
            return None;
        }
        self.advance(Goal::RegExp);
        Some(self.finish_arrow(context, params, is_async, start))
    }

    fn finish_arrow(
        &mut self,
        context: Context,
        params: Vec<Pattern>,
        is_async: bool,
        start: usize,
    ) -> Expression {
        let body_context = context.function_body(false, is_async);
        let params_span = Span::new(start, self.token.span.start);
        let (body, strict, declared_strict) = if self.at(Punctuator::OpenBrace) {
            self.advance(Goal::RegExp);
            let (statements, strict, declared) =
                self.parse_directive_prologue_and_body(body_context, Some(Punctuator::CloseBrace));
            self.expect(Punctuator::CloseBrace, Goal::Div);
            (ArrowBody::Block(statements), strict, declared)
        } else {
            let expression = self.parse_assignment(body_context);
            (ArrowBody::Expression(Box::new(expression)), body_context.strict, false)
        };
        self.recheck_parameters_after_body(&params, params_span, strict, declared_strict);
        Expression::Arrow(Box::new(Arrow {
            params,
            body,
            is_async,
            strict,
            span: self.span_from(start),
        }))
    }

    /// Refines an expression into an assignment target.
    ///
    /// The cover grammar's second half: an array or object literal becomes a destructuring pattern
    /// the moment an `=` proves it was one all along.
    ///
    /// # WARNING: UNWRAP THE PARENTHESES FIRST, THEN JUDGE -- AND THE ANSWER DEPENDS ON BOTH
    ///
    /// Parentheses are transparent for SOME targets and fatal for others, so neither "recurse
    /// through them" nor "reject them" is right:
    ///
    /// - `(eval) = 1` is a strict-mode error, so the eval/arguments check has to see THROUGH the
    ///   parentheses. Checking only a bare `Expression::Identifier` missed every parenthesized one.
    /// - `({} = 1)` is legal destructuring and `({}) = 1` is a SyntaxError, so an object or array
    ///   literal only covers a pattern when it is DIRECTLY the left-hand side.
    /// - `(x = y) = 1` is a SyntaxError, and this one is the trap: an AssignmentExpression refines
    ///   happily into a `Default` PATTERN, because `[a = 1] = b` is real destructuring. That
    ///   refinement is correct INSIDE a pattern and wrong at the top of a target, where there is no
    ///   pattern for a default to belong to.
    fn refine_to_assignment_target(
        &mut self,
        expression: Expression,
        context: Context,
    ) -> AssignmentTarget {
        let (inner, parenthesized) = parenthesized_inner(&expression);

        let acceptable = match inner {
            Expression::Identifier { .. } | Expression::Member { .. } => true,
            Expression::Array { .. } | Expression::Object { .. } => !parenthesized,
            _ => false,
        };
        if acceptable {
            AssignmentTarget::Pattern {
                pattern: self.refine_to_pattern(expression, context),
                parenthesized,
            }
        } else {
            let span = expression.span();
            self.early_error(span, "invalid assignment target");
            AssignmentTarget::Invalid(expression)
        }
    }

    /// A compound assignment (`+=`, `&&=`, ...) accepts only a SIMPLE target.
    ///
    /// `[a] = b` is destructuring; `[a] += b` is a SyntaxError. Treating both the same way accepts a
    /// program the standard rejects.
    fn refine_to_simple_target(&mut self, expression: Expression, context: Context) -> AssignmentTarget {
        let (inner, parenthesized) = parenthesized_inner(&expression);
        match inner {
            Expression::Identifier { .. } | Expression::Member { .. } => {
                AssignmentTarget::Pattern {
                    pattern: self.refine_to_pattern(expression, context),
                    parenthesized,
                }
            }
            _ => {
                let span = expression.span();
                self.early_error(span, "a compound assignment needs a simple target");
                AssignmentTarget::Invalid(expression)
            }
        }
    }

    /// `eval` and `arguments` may not be assigned to in strict code, in ANY target position.
    ///
    /// One function rather than a check at each site, because the sites are the problem: there
    /// are four ways to write a target that lands on an identifier -- the bare one, an array
    /// element, an object property's value, and a shorthand with a default -- and the rule had
    /// reached exactly one of them.
    fn refuse_strict_eval_or_arguments(&mut self, name: &str, span: Span, context: Context) {
        if context.strict && (name == "eval" || name == "arguments") {
            self.early_error(span, format!("`{name}` may not be assigned in strict mode"));
        }
    }

    /// Turns an expression into the pattern it was covering.
    ///
    /// Every failure reports and returns a placeholder rather than unwinding, so one bad element in
    /// a destructuring pattern costs its own diagnostic and not the rest of the file.
    fn refine_to_pattern(&mut self, expression: Expression, context: Context) -> Pattern {
        match expression {
            Expression::Identifier { name, span } => {
                self.refuse_strict_eval_or_arguments(&name, span, context);
                Pattern::Identifier { name, span }
            }
            Expression::Member { object, property, optional, span } => {
                if optional || chain_is_optional(&object) {
                    self.early_error(span, "an optional chain is not an assignment target");
                }
                Pattern::Member { object, property, optional, span }
            }
            Expression::Parenthesized { expression, .. } => self.refine_to_pattern(*expression, context),
            Expression::Assignment {
                operator: AssignmentOperator::Assign,
                target,
                value,
                span,
            } => {
                let inner = match *target {
                    AssignmentTarget::Pattern { pattern, .. } => pattern,
                    AssignmentTarget::Invalid(expression) => self.refine_to_pattern(expression, context),
                };
                Pattern::Default { target: Box::new(inner), value, span }
            }
            Expression::Array { elements, span } => {
                let count = elements.len();
                let mut refined = Vec::new();
                let mut rest = None;
                for (index, element) in elements.into_iter().enumerate() {
                    match element {
                        ArrayElement::Hole => refined.push(None),
                        ArrayElement::Expression(expression) => {
                            refined.push(Some(self.refine_to_pattern(expression, context)));
                        }
                        ArrayElement::Spread { argument, span, comma_after } => {
                            if index + 1 != count {
                                self.early_error(span, "a rest element must be the last element");
                            }
                            if comma_after {
                                self.early_error(
                                    span,
                                    "a rest element may not be followed by a comma",
                                );
                            }
                            let pattern = self.refine_to_pattern(argument, context);
                            if matches!(pattern, Pattern::Default { .. }) {
                                self.early_error(
                                    span,
                                    "a rest element may not have an initializer",
                                );
                            }
                            rest = Some(Box::new(Pattern::Rest {
                                argument: Box::new(pattern),
                                span,
                            }));
                        }
                    }
                }
                Pattern::Array { elements: refined, rest, span }
            }
            Expression::Object { properties, span } => {
                self.parked_cover_errors
                    .retain(|(parked, _)| parked.start < span.start || parked.end > span.end);
                let count = properties.len();
                let mut refined = Vec::new();
                let mut rest = None;
                for (index, property) in properties.into_iter().enumerate() {
                    match property {
                        ObjectProperty::Property { key, value, computed, shorthand, span } => {
                            let value = self.refine_to_pattern(value, context);
                            refined.push(ObjectPatternProperty { key, value, computed, shorthand, span });
                        }
                        ObjectProperty::CoverInitializedName { name, value, span } => {
                            self.refuse_strict_eval_or_arguments(&name, span, context);
                            refined.push(ObjectPatternProperty {
                                key: PropertyKey::Identifier { name: name.clone(), span },
                                value: Pattern::Default {
                                    target: Box::new(Pattern::Identifier { name, span }),
                                    value: Box::new(value),
                                    span,
                                },
                                computed: false,
                                shorthand: true,
                                span,
                            });
                        }
                        ObjectProperty::Spread { argument, span } => {
                            if index + 1 != count {
                                self.early_error(span, "a rest property must be the last property");
                            }
                            let pattern = self.refine_to_pattern(argument, context);
                            if matches!(pattern, Pattern::Default { .. }) {
                                self.early_error(
                                    span,
                                    "a rest property may not have an initializer",
                                );
                            }
                            rest = Some(Box::new(Pattern::Rest {
                                argument: Box::new(pattern),
                                span,
                            }));
                        }
                        ObjectProperty::Method { span, .. } => {
                            self.early_error(span, "a method may not appear in a destructuring pattern");
                        }
                    }
                }
                Pattern::Object { properties: refined, rest, span }
            }
            other => {
                let span = other.span();
                self.early_error(span, "this expression cannot be a destructuring target");
                Pattern::Identifier { name: String::new(), span }
            }
        }
    }
}

/// The first character boundary at or after `index`, clamped to the end of `source`.
///
/// Error recovery walks forward by a byte count it computed from spans, and a byte offset is not
/// necessarily a character boundary. Slicing at one panics, so recovery has to round UP to a real
/// boundary -- forward rather than back, because going back could re-scan the text that failed and
/// stall.
fn next_char_boundary(source: &str, index: usize) -> usize {
    let mut index = index.min(source.len());
    while index < source.len() && !source.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Words that can never be an identifier, in any mode.
///
/// # WARNING: `true`, `false` AND `null` BELONG HERE, THOUGH THEY ARE LITERALS
///
/// They were left out at first on the reasoning that the grammar produces them as literals, so the
/// literal path would always catch them. It does -- **for the unescaped spelling only.** `f\u{61}lse`
/// is not the literal, because a Unicode escape disqualifies a token from being a keyword; it falls
/// through to the identifier path, and without these three entries it became a perfectly ordinary
/// identifier reference. `ReservedWord` includes them, so an escaped one is a SyntaxError.
///
/// **`super` IS ON THIS LIST AND WAS ON NO LIST AT ALL**, so `var super = 1;` parsed and ran.
/// It had been held out deliberately, back when classes were absent surface: reporting it as a
/// misused reserved word made 18 published absences read as parser defects. Classes now work, so
/// the reason expired -- and what was left was a keyword the grammar reserves being bound as an
/// ordinary name. `super.x` and `super()` are unaffected: `parse_primary` recognizes the
/// UNESCAPED spelling before the identifier path is reached, and the escaped spelling
/// (`super`) is precisely what has to be refused.
const ALWAYS_RESERVED: &[&str] = &[
    "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do",
    "else", "enum", "export", "extends", "false", "finally", "for", "function", "if", "import",
    "in", "instanceof", "new", "null", "return", "super", "switch", "this", "throw", "true", "try",
    "typeof", "var", "void", "while", "with",
];

/// Words reserved only in strict mode.
///
/// **The list is short and every entry is legal sloppy code**, which is why it is a MODE question
/// rather than a lexer one: `var package = 1;` is a perfectly good script and a SyntaxError three
/// characters after a `"use strict"` directive.
const STRICT_RESERVED: &[&str] =
    &["implements", "interface", "let", "package", "private", "protected", "public", "static", "yield"];

/// Peels `( ... )` off an expression, reporting whether any came off.
///
/// **EVERY RULE THAT ASKS WHAT AN OPERAND *IS* HAS TO START HERE.** A parenthesized expression
/// derives its contents, so `(a)++`, `(a) = b`, `(a) += b` and `delete (a)` all ask about `a`. This
/// parser wrote the same `while let` three times and the fourth rule -- `delete`'s strict-mode early
/// error -- did not write it at all, which is the shape where a rule with several implementations
/// gains a case in none of them. One function, so the next rule to need it cannot quietly skip it.
///
/// The bool is not decoration. `[a] = b` destructures and `([a]) = b` does not, so two callers
/// need to know that a layer came off as well as what was under it.
fn parenthesized_inner(expression: &Expression) -> (&Expression, bool) {
    let mut inner = expression;
    let mut parenthesized = false;
    while let Expression::Parenthesized { expression, .. } = inner {
        inner = expression;
        parenthesized = true;
    }
    (inner, parenthesized)
}

/// Whether an expression is part of an OPTIONAL CHAIN, at any depth of its head.
///
/// `a?.b.c` and `a?.b().c` are optional expressions whose OUTERMOST link is ordinary, so the flag
/// on the node being refined answers `false` for both. The question the standard asks is about the
/// whole `OptionalExpression`, which is what this walk reconstructs.
fn chain_is_optional(expression: &Expression) -> bool {
    match expression {
        Expression::Member { object, optional, .. } => *optional || chain_is_optional(object),
        Expression::Call { callee, optional, .. } => *optional || chain_is_optional(callee),
        _ => false,
    }
}

/// Whether `name` may not be used as an identifier in this context.
///
/// Contextual keywords -- `of`, `as`, `from`, `get`, `set`, `async`, `target`, `meta` -- are
/// **absent from both lists on purpose.** They are ordinary identifiers that the parser recognizes
/// positionally, and reserving them would reject a great deal of legal code.
fn reserved_in(name: &str, context: Context) -> bool {
    if ALWAYS_RESERVED.contains(&name) {
        return true;
    }
    if context.strict && STRICT_RESERVED.contains(&name) {
        return true;
    }
    (context.allow_yield && name == "yield") || (context.allow_await && name == "await")
}

/// Visits every name a binding pattern introduces.
fn collect_bound_names<'p>(pattern: &'p Pattern, visit: &mut impl FnMut(&'p str, Span)) {
    match pattern {
        Pattern::Identifier { name, span } => visit(name, *span),
        Pattern::Default { target, .. } | Pattern::Rest { argument: target, .. } => {
            collect_bound_names(target, visit);
        }
        Pattern::Array { elements, rest, .. } => {
            for element in elements.iter().flatten() {
                collect_bound_names(element, visit);
            }
            if let Some(rest) = rest {
                collect_bound_names(rest, visit);
            }
        }
        Pattern::Object { properties, rest, .. } => {
            for property in properties {
                collect_bound_names(&property.value, visit);
            }
            if let Some(rest) = rest {
                collect_bound_names(rest, visit);
            }
        }
        Pattern::Member { .. } => {}
    }
}

/// Whether a statement is a function declaration wearing any number of labels.
///
/// The recursion is the point: `a: b: c: function f() {}` is still a labelled function, and a check
/// that looked only one level down would be defeated by a second label.
fn is_labelled_function(statement: &Statement) -> bool {
    match statement {
        Statement::Labeled { body, .. } => match &**body {
            Statement::Function(_) => true,
            inner @ Statement::Labeled { .. } => is_labelled_function(inner),
            _ => false,
        },
        _ => false,
    }
}
