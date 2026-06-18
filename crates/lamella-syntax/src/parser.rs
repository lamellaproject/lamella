//! The parser: building a syntax tree from the token stream.

use crate::ast::{
    AssignmentOperator, BinaryOperator, Expr, ExprKind, Literal, PostfixOperator, PredefinedType,
    TypeRef, TypeRefKind, TypeTestOperation, UnaryOperator,
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

    /// Prefix unary operators, including pre-increment and pre-decrement (14.6).
    fn parse_unary(&mut self) -> Expr {
        let Some(operator) = self.current_punctuator().and_then(prefix_operator) else {
            return self.parse_postfix();
        };
        let start = self.current().span.start;
        self.bump();
        let operand = self.parse_unary();
        let span = Span::new(start, operand.span.end);
        Expr::new(
            ExprKind::Unary {
                operator,
                operand: Box::new(operand),
            },
            span,
        )
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
            TokenKind::RealLiteral { suffix } => {
                self.bump();
                Expr::new(ExprKind::Literal(Literal::Real { suffix }), span)
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
            arguments.push(self.parse_expression());
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
    fn parse_type(&mut self) -> TypeRef {
        let start = self.current().span.start;
        let base = if let Some(predefined) = predefined_type(&self.current().kind) {
            let span = self.current().span;
            self.bump();
            TypeRef::new(TypeRefKind::Predefined(predefined), span)
        } else if matches!(self.current().kind, TokenKind::Identifier(_)) {
            self.parse_type_name()
        } else {
            let at = self.current().span.start;
            self.report(DiagnosticKind::TypeExpected, Span::empty_at(at));
            return TypeRef::new(TypeRefKind::Error, Span::empty_at(at));
        };
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
            ExprKind::This => String::from("this"),
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
}
