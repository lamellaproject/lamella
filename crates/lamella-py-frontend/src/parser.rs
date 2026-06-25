//! A recursive-descent parser for the Python subset.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::ast::{
    Assign, BinOp, BoolOp, CmpOp, Expr, FuncDef, ModuleAst, ParamDef, Stmt, UnaryOp,
};
use crate::lexer::{Tok, Token};

/// The binary operator of an augmented-assignment token (`+=`, `<<=`, ...), or `None`
/// if the token is not one.
fn aug_assign_op(tok: &Tok) -> Option<BinOp> {
    Some(match tok {
        Tok::PlusEq => BinOp::Add,
        Tok::MinusEq => BinOp::Sub,
        Tok::StarEq => BinOp::Mul,
        Tok::SlashSlashEq => BinOp::FloorDiv,
        Tok::PercentEq => BinOp::Mod,
        Tok::AmperEq => BinOp::BitAnd,
        Tok::PipeEq => BinOp::BitOr,
        Tok::CaretEq => BinOp::BitXor,
        Tok::LtLtEq => BinOp::LShift,
        Tok::GtGtEq => BinOp::RShift,
        _ => return None,
    })
}

/// A parse failure: the offending line and a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// The 1-based source line the error was detected on.
    pub line: u32,
    /// What went wrong.
    pub message: String,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

/// Parse a token stream (ending in [`Tok::Eof`]) into a module AST.
pub fn parse(tokens: Vec<Token>) -> Result<ModuleAst, ParseError> {
    let mut parser = Parser { tokens, pos: 0 };
    parser.parse_module()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].kind
    }

    fn current_line(&self) -> u32 {
        self.tokens[self.pos].line
    }

    fn advance(&mut self) {
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
    }

    fn at(&self, kind: &Tok) -> bool {
        self.peek() == kind
    }

    fn eat(&mut self, kind: &Tok) -> bool {
        if self.at(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            line: self.current_line(),
            message: message.into(),
        }
    }

    fn expect(&mut self, kind: &Tok, what: &str) -> Result<(), ParseError> {
        if self.at(kind) {
            self.advance();
            Ok(())
        } else {
            Err(self.error(format!("expected {what}")))
        }
    }

    fn expect_name(&mut self) -> Result<String, ParseError> {
        if let Tok::Name(name) = self.peek() {
            let name = name.clone();
            self.advance();
            Ok(name)
        } else if let Tok::Reserved(word) = self.peek() {
            Err(self.error(format!(
                "'{word}' is a reserved keyword and cannot be used as a name"
            )))
        } else {
            Err(self.error("expected a name"))
        }
    }

    fn expect_newline(&mut self) -> Result<(), ParseError> {
        self.expect(&Tok::Newline, "end of line")
    }


    fn parse_module(&mut self) -> Result<ModuleAst, ParseError> {
        let mut body = Vec::new();
        while !self.at(&Tok::Eof) {
            body.push(self.parse_statement()?);
        }
        Ok(ModuleAst { body })
    }

    fn parse_statement(&mut self) -> Result<Stmt, ParseError> {
        match self.peek() {
            Tok::KwDef => self.parse_funcdef(),
            Tok::KwIf => self.parse_if(),
            Tok::KwWhile => self.parse_while(),
            Tok::KwFor => self.parse_for(),
            _ => self.parse_small_stmt(),
        }
    }

    /// A non-compound statement: `return`, an assignment, or an expression
    /// statement. Consumes the trailing [`Tok::Newline`].
    fn parse_small_stmt(&mut self) -> Result<Stmt, ParseError> {
        if self.at(&Tok::KwReturn) {
            self.parse_return()
        } else if self.at(&Tok::KwBreak) {
            self.advance();
            self.expect_newline()?;
            Ok(Stmt::Break)
        } else if self.at(&Tok::KwContinue) {
            self.advance();
            self.expect_newline()?;
            Ok(Stmt::Continue)
        } else if self.at(&Tok::KwPass) {
            self.advance();
            self.expect_newline()?;
            Ok(Stmt::Pass)
        } else {
            self.parse_assign_or_expr()
        }
    }

    fn parse_return(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwReturn, "'return'")?;
        let value = if self.at(&Tok::Newline) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        self.expect_newline()?;
        Ok(Stmt::Return(value))
    }

    /// `assignment_stmt`, `annotated_assignment_stmt`, or `expression_stmt`. The
    /// statement is parsed as an expression first; a following `:` or `=` then
    /// reinterprets it as an (annotated) assignment, restricted to a bare name
    /// as the target.
    fn parse_assign_or_expr(&mut self) -> Result<Stmt, ParseError> {
        let target_line = self.current_line();
        let expr = self.parse_expr()?;
        if let Some(op) = aug_assign_op(self.peek()) {
            let target = self.target_name(expr, target_line)?;
            self.advance();
            let value = self.parse_expr()?;
            self.expect_newline()?;
            return Ok(Stmt::Assign(Assign {
                target: target.clone(),
                annotation: None,
                value: Some(Expr::Binary {
                    op,
                    lhs: Box::new(Expr::Name(target)),
                    rhs: Box::new(value),
                }),
            }));
        }
        match self.peek() {
            Tok::Colon => {
                let target = self.target_name(expr, target_line)?;
                self.advance();
                let annotation = Some(self.parse_expr()?);
                let value = if self.eat(&Tok::Assign) {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                self.expect_newline()?;
                Ok(Stmt::Assign(Assign {
                    target,
                    annotation,
                    value,
                }))
            }
            Tok::Assign => {
                let mut targets = vec![self.target_name(expr, target_line)?];
                self.advance();
                let mut value = self.parse_expr()?;
                while self.at(&Tok::Assign) {
                    targets.push(self.target_name(value, target_line)?);
                    self.advance();
                    value = self.parse_expr()?;
                }
                self.expect_newline()?;
                if targets.len() == 1 {
                    Ok(Stmt::Assign(Assign {
                        target: targets.pop().unwrap(),
                        annotation: None,
                        value: Some(value),
                    }))
                } else {
                    Ok(Stmt::MultiAssign { targets, value })
                }
            }
            _ => {
                self.expect_newline()?;
                Ok(Stmt::Expr(expr))
            }
        }
    }

    /// Require an assignment target to be a bare name (attribute, subscript, and
    /// tuple targets are not supported in this subset).
    fn target_name(&self, expr: Expr, line: u32) -> Result<String, ParseError> {
        match expr {
            Expr::Name(name) => Ok(name),
            _ => Err(ParseError {
                line,
                message: String::from(
                    "only a bare name is a valid assignment target (attribute, subscript, \
                     and tuple targets are not supported in this subset)",
                ),
            }),
        }
    }

    fn parse_if(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwIf, "'if'")?;
        let test = self.parse_expr()?;
        self.expect(&Tok::Colon, "':'")?;
        let body = self.parse_suite()?;
        let orelse = self.parse_elif_else()?;
        Ok(Stmt::If { test, body, orelse })
    }

    /// The `("elif" ...)* ["else" ...]` tail of an `if`. An `elif` is desugared
    /// into a nested `if` in the enclosing clause's `orelse`, which keeps the AST
    /// to a single conditional node shape.
    fn parse_elif_else(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if self.at(&Tok::KwElif) {
            self.advance();
            let test = self.parse_expr()?;
            self.expect(&Tok::Colon, "':'")?;
            let body = self.parse_suite()?;
            let orelse = self.parse_elif_else()?;
            Ok(vec![Stmt::If { test, body, orelse }])
        } else if self.eat(&Tok::KwElse) {
            self.expect(&Tok::Colon, "':'")?;
            self.parse_suite()
        } else {
            Ok(Vec::new())
        }
    }

    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwFor, "'for'")?;
        let target = self.expect_name()?;
        self.expect(&Tok::KwIn, "'in'")?;
        let iter = self.parse_expr()?;
        let (start, stop, step) = self.range_bounds(iter)?;
        self.expect(&Tok::Colon, "':'")?;
        let body = self.parse_suite()?;
        let orelse = self.parse_loop_else()?;
        Ok(Stmt::For {
            target,
            start,
            stop,
            step,
            body,
            orelse,
        })
    }

    /// Only `range(stop)`, `range(start, stop)`, or `range(start, stop, step)` are
    /// iterable in this subset; pull out the bounds (a missing start is `0`, a
    /// missing step is `1`). The step must be a non-zero integer literal.
    fn range_bounds(&self, iter: Expr) -> Result<(Expr, Expr, i64), ParseError> {
        if let Expr::Call { func, args } = iter {
            if matches!(&*func, Expr::Name(n) if n == "range") {
                let mut args = args;
                match args.len() {
                    1 => return Ok((Expr::Int(0), args.pop().unwrap(), 1)),
                    2 => {
                        let stop = args.pop().unwrap();
                        let start = args.pop().unwrap();
                        return Ok((start, stop, 1));
                    }
                    3 => {
                        let step_expr = args.pop().unwrap();
                        let stop = args.pop().unwrap();
                        let start = args.pop().unwrap();
                        let Expr::Int(step) = step_expr else {
                            return Err(
                                self.error("the range() step must be an integer literal")
                            );
                        };
                        if step == 0 {
                            return Err(self.error("range() step must not be zero"));
                        }
                        return Ok((start, stop, step));
                    }
                    _ => {}
                }
            }
        }
        Err(self.error(
            "'for' iterates only range(stop), range(start, stop), or range(start, stop, step) \
             in this subset",
        ))
    }

    fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwWhile, "'while'")?;
        let test = self.parse_expr()?;
        self.expect(&Tok::Colon, "':'")?;
        let body = self.parse_suite()?;
        let orelse = self.parse_loop_else()?;
        Ok(Stmt::While { test, body, orelse })
    }

    /// An optional `else:` suite on a `while`/`for` (run when the loop exits without
    /// `break`). Empty when the clause is absent.
    fn parse_loop_else(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if self.eat(&Tok::KwElse) {
            self.expect(&Tok::Colon, "':'")?;
            self.parse_suite()
        } else {
            Ok(Vec::new())
        }
    }

    /// `suite: stmt_list NEWLINE | NEWLINE INDENT statement+ DEDENT`. The
    /// single-line form holds one simple statement (no `;`-separated list).
    fn parse_suite(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if self.eat(&Tok::Newline) {
            self.expect(&Tok::Indent, "an indented block")?;
            let mut body = Vec::new();
            while !self.at(&Tok::Dedent) && !self.at(&Tok::Eof) {
                body.push(self.parse_statement()?);
            }
            self.expect(&Tok::Dedent, "a dedent ending the block")?;
            Ok(body)
        } else {
            Ok(vec![self.parse_small_stmt()?])
        }
    }

    fn parse_funcdef(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwDef, "'def'")?;
        let name = self.expect_name()?;
        self.expect(&Tok::LParen, "'(' after the function name")?;
        let params = self.parse_params()?;
        self.expect(&Tok::RParen, "')'")?;
        let ret = if self.eat(&Tok::Arrow) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect(&Tok::Colon, "':'")?;
        let body = self.parse_suite()?;
        Ok(Stmt::FuncDef(FuncDef {
            name,
            params,
            ret,
            body,
        }))
    }

    /// `parameter ("," parameter)* [","]`, where `parameter: identifier [":"
    /// expression]`. Default values, `*args`/`**kwargs`, and positional-only `/`
    /// are out of the subset and rejected explicitly.
    fn parse_params(&mut self) -> Result<Vec<ParamDef>, ParseError> {
        let mut params = Vec::new();
        if self.at(&Tok::RParen) {
            return Ok(params);
        }
        loop {
            if self.at(&Tok::Star) || self.at(&Tok::DoubleSlash) {
                return Err(self.error(
                    "variadic and positional-only parameters are not supported in this subset",
                ));
            }
            let name = self.expect_name()?;
            let annotation = if self.eat(&Tok::Colon) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            if self.at(&Tok::Assign) {
                return Err(
                    self.error("default parameter values are not supported in this subset")
                );
            }
            params.push(ParamDef { name, annotation });
            if self.eat(&Tok::Comma) {
                if self.at(&Tok::RParen) {
                    break;
                }
                continue;
            }
            break;
        }
        Ok(params)
    }


    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_conditional()
    }

    /// `conditional: or_test ["if" or_test "else" conditional]` -- the ternary
    /// `body if test else orelse`, the lowest-precedence expression form, with the
    /// `else` branch right-associative.
    fn parse_conditional(&mut self) -> Result<Expr, ParseError> {
        let body = self.parse_or()?;
        if self.at(&Tok::KwIf) {
            self.advance();
            let test = self.parse_or()?;
            if !self.eat(&Tok::KwElse) {
                return Err(self.error("expected 'else' in a conditional expression"));
            }
            let orelse = self.parse_conditional()?;
            Ok(Expr::Conditional {
                test: Box::new(test),
                body: Box::new(body),
                orelse: Box::new(orelse),
            })
        } else {
            Ok(body)
        }
    }

    /// `or_test: and_test ("or" and_test)*` -- left-associative, just above the
    /// conditional.
    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and()?;
        while self.at(&Tok::KwOr) {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Expr::BoolBinary {
                op: BoolOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `and_test: not_test ("and" not_test)*` -- left-associative, just above `or`.
    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_not()?;
        while self.at(&Tok::KwAnd) {
            self.advance();
            let rhs = self.parse_not()?;
            lhs = Expr::BoolBinary {
                op: BoolOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `not_test: "not" not_test | comparison` -- right-associative, just above `and`
    /// and below a comparison (so `not a < b` is `not (a < b)`).
    fn parse_not(&mut self) -> Result<Expr, ParseError> {
        if self.at(&Tok::KwNot) {
            self.advance();
            let operand = self.parse_not()?;
            Ok(Expr::Not {
                operand: Box::new(operand),
            })
        } else {
            self.parse_comparison()
        }
    }

    /// A comparison, including Python's chains (`a < b < c`), which desugar to the
    /// `and` of the adjacent comparisons -- `(a < b) and (b < c)`. A shared middle
    /// operand is re-evaluated per comparison (exact for the side-effect-free
    /// operands chains typically use, e.g. `0 <= i < n`).
    fn parse_comparison(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_bitor()?;
        let mut chain: Option<Expr> = None;
        while let Some(op) = self.peek_cmp_op() {
            self.advance();
            let rhs = self.parse_bitor()?;
            let cmp = Expr::Compare {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs.clone()),
            };
            chain = Some(match chain {
                None => cmp,
                Some(prev) => Expr::BoolBinary {
                    op: BoolOp::And,
                    lhs: Box::new(prev),
                    rhs: Box::new(cmp),
                },
            });
            lhs = rhs;
        }
        Ok(chain.unwrap_or(lhs))
    }

    fn peek_cmp_op(&self) -> Option<CmpOp> {
        match self.peek() {
            Tok::EqEq => Some(CmpOp::Eq),
            Tok::NotEq => Some(CmpOp::Ne),
            Tok::Lt => Some(CmpOp::Lt),
            Tok::Le => Some(CmpOp::Le),
            Tok::Gt => Some(CmpOp::Gt),
            Tok::Ge => Some(CmpOp::Ge),
            _ => None,
        }
    }

    /// `or_expr: xor_expr ("|" xor_expr)*` -- bitwise OR, left-associative (Python
    /// precedence: just below comparison, just above `^`).
    fn parse_bitor(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_bitxor()?;
        while matches!(self.peek(), Tok::Pipe) {
            self.advance();
            let rhs = self.parse_bitxor()?;
            lhs = Expr::Binary {
                op: BinOp::BitOr,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `xor_expr: and_expr ("^" and_expr)*` -- bitwise XOR, left-associative.
    fn parse_bitxor(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_bitand()?;
        while matches!(self.peek(), Tok::Caret) {
            self.advance();
            let rhs = self.parse_bitand()?;
            lhs = Expr::Binary {
                op: BinOp::BitXor,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `and_expr: shift_expr ("&" shift_expr)*` -- bitwise AND, left-associative.
    fn parse_bitand(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_shift()?;
        while matches!(self.peek(), Tok::Amper) {
            self.advance();
            let rhs = self.parse_shift()?;
            lhs = Expr::Binary {
                op: BinOp::BitAnd,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `shift_expr: a_expr (("<<" | ">>") a_expr)*` -- left-associative (Python
    /// precedence: just above additive, just below `&`).
    fn parse_shift(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Tok::LtLt => BinOp::LShift,
                Tok::GtGt => BinOp::RShift,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_additive()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `a_expr: m_expr | a_expr "+" m_expr | a_expr "-" m_expr` -- left-associative.
    fn parse_additive(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `m_expr` for the subset's operators (`*`, `//`, `%`) -- left-associative.
    /// True division `/` produces a float and is rejected.
    fn parse_multiplicative(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::DoubleSlash => BinOp::FloorDiv,
                Tok::Percent => BinOp::Mod,
                Tok::Slash => {
                    return Err(self.error(
                        "true division '/' is not supported in this subset; use '//' for \
                         integer floor division",
                    ));
                }
                _ => break,
            };
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `u_expr: power | ("-" | "+" | "~") u_expr` -- unary minus, plus, and bitwise
    /// inversion, right-associative. A unary operator applied directly to an integer
    /// literal is folded to a constant (`-3`, `~3`); otherwise it becomes a `Unary`.
    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        let op = match self.peek() {
            Tok::Minus => UnaryOp::Neg,
            Tok::Plus => UnaryOp::Pos,
            Tok::Tilde => UnaryOp::Invert,
            _ => return self.parse_trailer(),
        };
        let line = self.current_line();
        self.advance();
        let operand = self.parse_unary()?;
        if let Expr::Int(value) = operand {
            let folded = match op {
                UnaryOp::Neg => value.checked_neg().ok_or_else(|| ParseError {
                    line,
                    message: String::from("integer literal out of range"),
                })?,
                UnaryOp::Pos => value,
                UnaryOp::Invert => !value,
            };
            return Ok(Expr::Int(folded));
        }
        Ok(Expr::Unary {
            op,
            operand: Box::new(operand),
        })
    }

    /// Postfix attribute reference (`primary "." identifier`), call (`primary "("
    /// [args] ")"`), and subscript (`primary "[" expr "]"`) -- all left-associative and
    /// binding tightest.
    fn parse_trailer(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_atom()?;
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.advance();
                    let attr = self.expect_name()?;
                    expr = Expr::Attribute {
                        value: Box::new(expr),
                        attr,
                    };
                }
                Tok::LParen => {
                    self.advance();
                    let args = self.parse_args()?;
                    self.expect(&Tok::RParen, "')' closing the call")?;
                    expr = Expr::Call {
                        func: Box::new(expr),
                        args,
                    };
                }
                Tok::LBracket => {
                    self.advance();
                    let index = self.parse_expr()?;
                    self.expect(&Tok::RBracket, "']' closing the subscript")?;
                    expr = Expr::Subscript {
                        value: Box::new(expr),
                        index: Box::new(index),
                    };
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_args(&mut self) -> Result<Vec<Expr>, ParseError> {
        let mut args = Vec::new();
        if self.at(&Tok::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.parse_expr()?);
            if self.eat(&Tok::Comma) {
                if self.at(&Tok::RParen) {
                    break;
                }
                continue;
            }
            break;
        }
        Ok(args)
    }

    fn parse_atom(&mut self) -> Result<Expr, ParseError> {
        match self.peek().clone() {
            Tok::Int(value) => {
                self.advance();
                Ok(Expr::Int(value))
            }
            Tok::Str(value) => {
                self.advance();
                let mut joined = value;
                while let Tok::Str(next) = self.peek().clone() {
                    joined.push_str(&next);
                    self.advance();
                }
                Ok(Expr::Str(joined))
            }
            Tok::KwTrue => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            Tok::KwFalse => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            Tok::KwNone => {
                self.advance();
                Ok(Expr::None)
            }
            Tok::Name(name) => {
                self.advance();
                Ok(Expr::Name(name))
            }
            Tok::LParen => {
                self.advance();
                let inner = self.parse_expr()?;
                self.expect(&Tok::RParen, "')'")?;
                Ok(inner)
            }
            Tok::Reserved(word) => Err(self.error(format!(
                "'{word}' is a reserved keyword not supported in this subset"
            ))),
            _ => Err(self.error("expected an expression")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn parse_src(source: &str) -> Result<ModuleAst, ParseError> {
        parse(tokenize(source).expect("tokenizes"))
    }

    fn parse_ok(source: &str) -> ModuleAst {
        parse_src(source).expect("parses")
    }

    #[test]
    fn attribute_access_is_an_expression_statement() {
        let module = parse_ok("obj.x\n");
        assert_eq!(
            module.body,
            vec![Stmt::Expr(Expr::Attribute {
                value: Box::new(Expr::Name("obj".into())),
                attr: "x".into(),
            })]
        );
    }

    #[test]
    fn annotated_assignment_with_value() {
        let module = parse_ok("a: int = 0\n");
        assert_eq!(
            module.body,
            vec![Stmt::Assign(Assign {
                target: "a".into(),
                annotation: Some(Expr::Name("int".into())),
                value: Some(Expr::Int(0)),
            })]
        );
    }

    #[test]
    fn bare_annotation_has_no_value() {
        let module = parse_ok("a: int\n");
        let Stmt::Assign(assign) = &module.body[0] else {
            panic!("expected an assignment");
        };
        assert_eq!(assign.value, None);
        assert!(assign.annotation.is_some());
    }

    #[test]
    fn augmented_assignment_desugars_to_a_binary_assign() {
        let module = parse_ok("x += 5\n");
        let Stmt::Assign(assign) = &module.body[0] else {
            panic!("expected an assignment");
        };
        assert_eq!(assign.target, "x");
        assert_eq!(assign.annotation, None);
        let Some(Expr::Binary { op, lhs, .. }) = &assign.value else {
            panic!("expected a binary value");
        };
        assert_eq!(*op, BinOp::Add);
        assert_eq!(**lhs, Expr::Name("x".into()));
    }

    #[test]
    fn all_augmented_operators_map_to_their_binops() {
        for (src, want) in [
            ("x += 1\n", BinOp::Add),
            ("x -= 1\n", BinOp::Sub),
            ("x *= 1\n", BinOp::Mul),
            ("x //= 1\n", BinOp::FloorDiv),
            ("x %= 1\n", BinOp::Mod),
            ("x &= 1\n", BinOp::BitAnd),
            ("x |= 1\n", BinOp::BitOr),
            ("x ^= 1\n", BinOp::BitXor),
            ("x <<= 1\n", BinOp::LShift),
            ("x >>= 1\n", BinOp::RShift),
        ] {
            let module = parse_ok(src);
            let Stmt::Assign(assign) = &module.body[0] else {
                panic!("expected an assignment for {src:?}");
            };
            let Some(Expr::Binary { op, .. }) = &assign.value else {
                panic!("expected a binary value for {src:?}");
            };
            assert_eq!(*op, want, "for source {src:?}");
        }
    }

    #[test]
    fn boolean_precedence_is_or_below_and_below_not() {
        let module = parse_ok("a or b and c\n");
        let Stmt::Expr(Expr::BoolBinary { op, rhs, .. }) = &module.body[0] else {
            panic!("expected a top-level boolean expression");
        };
        assert_eq!(*op, BoolOp::Or);
        assert!(matches!(
            **rhs,
            Expr::BoolBinary {
                op: BoolOp::And,
                ..
            }
        ));
    }

    #[test]
    fn not_binds_below_a_comparison() {
        let module = parse_ok("not a < b\n");
        let Stmt::Expr(Expr::Not { operand }) = &module.body[0] else {
            panic!("expected a top-level `not`");
        };
        assert!(matches!(**operand, Expr::Compare { .. }));
    }

    #[test]
    fn conditional_expression_is_right_associative() {
        let module = parse_ok("a if p else b if q else c\n");
        let Stmt::Expr(Expr::Conditional { orelse, .. }) = &module.body[0] else {
            panic!("expected a conditional expression");
        };
        assert!(matches!(**orelse, Expr::Conditional { .. }));
    }

    #[test]
    fn for_over_range_extracts_its_bounds() {
        let module = parse_ok("for i in range(5):\n    x = i\n");
        let Stmt::For {
            target, start, stop, ..
        } = &module.body[0]
        else {
            panic!("expected a for statement");
        };
        assert_eq!(target, "i");
        assert_eq!(*start, Expr::Int(0));
        assert_eq!(*stop, Expr::Int(5));
        let two = parse_ok("for i in range(2, 9):\n    x = i\n");
        let Stmt::For { start, stop, .. } = &two.body[0] else {
            panic!("expected a for statement");
        };
        assert_eq!(*start, Expr::Int(2));
        assert_eq!(*stop, Expr::Int(9));
    }

    #[test]
    fn for_over_a_non_range_is_rejected() {
        assert!(parse_src("for x in stuff:\n    y = x\n").is_err());
    }

    #[test]
    fn pass_parses_to_a_no_op() {
        assert!(matches!(parse_ok("pass\n").body[0], Stmt::Pass));
    }

    #[test]
    fn loops_take_an_optional_else_clause() {
        let with = parse_ok("for i in range(3):\n    pass\nelse:\n    pass\n");
        let Stmt::For { orelse, .. } = &with.body[0] else {
            panic!("expected a for loop");
        };
        assert_eq!(orelse.len(), 1);
        let without = parse_ok("while x:\n    pass\n");
        let Stmt::While { orelse, .. } = &without.body[0] else {
            panic!("expected a while loop");
        };
        assert!(orelse.is_empty());
    }

    #[test]
    fn subscript_parses_left_associative() {
        let module = parse_ok("s[i]\n");
        let Stmt::Expr(Expr::Subscript { value, index }) = &module.body[0] else {
            panic!("expected a subscript");
        };
        assert!(matches!(&**value, Expr::Name(n) if n == "s"));
        assert!(matches!(&**index, Expr::Name(n) if n == "i"));
        let chained = parse_ok("m[i][j]\n");
        let Stmt::Expr(Expr::Subscript { value, .. }) = &chained.body[0] else {
            panic!("expected a subscript");
        };
        assert!(matches!(&**value, Expr::Subscript { .. }));
    }

    #[test]
    fn adjacent_string_literals_concatenate() {
        assert_eq!(parse_ok("\"ab\" \"cd\"\n").body[0], Stmt::Expr(Expr::Str("abcd".into())));
        assert!(matches!(
            parse_ok("\"ab\" + \"cd\"\n").body[0],
            Stmt::Expr(Expr::Binary { .. })
        ));
    }

    #[test]
    fn multiple_assignment_collects_targets() {
        let module = parse_ok("a = b = c = 0\n");
        let Stmt::MultiAssign { targets, value } = &module.body[0] else {
            panic!("expected a multiple assignment");
        };
        assert_eq!(targets, &["a", "b", "c"]);
        assert_eq!(*value, Expr::Int(0));
        assert!(matches!(parse_ok("a = 0\n").body[0], Stmt::Assign(_)));
    }

    #[test]
    fn chained_comparison_desugars_to_and() {
        let module = parse_ok("a < b < c\n");
        let Stmt::Expr(Expr::BoolBinary { op, lhs, rhs }) = &module.body[0] else {
            panic!("expected a boolean expression");
        };
        assert_eq!(*op, BoolOp::And);
        assert!(matches!(**lhs, Expr::Compare { .. }));
        assert!(matches!(**rhs, Expr::Compare { .. }));
        let single = parse_ok("a < b\n");
        assert!(matches!(single.body[0], Stmt::Expr(Expr::Compare { .. })));
    }

    #[test]
    fn break_and_continue_parse() {
        let module = parse_ok("while x:\n    break\n    continue\n");
        let Stmt::While { body, .. } = &module.body[0] else {
            panic!("expected a while");
        };
        assert!(matches!(body[0], Stmt::Break));
        assert!(matches!(body[1], Stmt::Continue));
    }

    #[test]
    fn range_with_a_step_is_extracted_and_validated() {
        let module = parse_ok("for i in range(0, 10, 2):\n    x = i\n");
        let Stmt::For { step, .. } = &module.body[0] else {
            panic!("expected a for");
        };
        assert_eq!(*step, 2);
        assert!(parse_src("for i in range(0, 10, n):\n    x = i\n").is_err());
        assert!(parse_src("for i in range(0, 10, 0):\n    x = i\n").is_err());
    }

    #[test]
    fn precedence_matches_the_reference() {
        let module = parse_ok("1 + 2 * 3\n");
        let Stmt::Expr(Expr::Binary { op, rhs, .. }) = &module.body[0] else {
            panic!("expected a binary expression at the top");
        };
        assert_eq!(*op, BinOp::Add);
        assert!(matches!(
            **rhs,
            Expr::Binary {
                op: BinOp::Mul,
                ..
            }
        ));
    }

    #[test]
    fn unary_minus_folds_into_a_literal() {
        let module = parse_ok("x = -3\n");
        let Stmt::Assign(assign) = &module.body[0] else {
            panic!("expected an assignment");
        };
        assert_eq!(assign.value, Some(Expr::Int(-3)));
    }

    #[test]
    fn function_with_annotations_and_a_while_loop() {
        let src = "\
def fib(n: int) -> int:
    a: int = 0
    while n > 0:
        a = a + 1
        n = n - 1
    return a
";
        let module = parse_ok(src);
        let Stmt::FuncDef(func) = &module.body[0] else {
            panic!("expected a function definition");
        };
        assert_eq!(func.name, "fib");
        assert_eq!(func.params.len(), 1);
        assert_eq!(func.params[0].name, "n");
        assert_eq!(func.params[0].annotation, Some(Expr::Name("int".into())));
        assert_eq!(func.ret, Some(Expr::Name("int".into())));
        assert!(matches!(func.body.last(), Some(Stmt::Return(Some(_)))));
        assert!(func.body.iter().any(|s| matches!(s, Stmt::While { .. })));
    }

    #[test]
    fn elif_desugars_to_a_nested_if() {
        let src = "\
if a:
    x = 1
elif b:
    x = 2
else:
    x = 3
";
        let module = parse_ok(src);
        let Stmt::If { orelse, .. } = &module.body[0] else {
            panic!("expected an if");
        };
        assert_eq!(orelse.len(), 1);
        assert!(matches!(orelse[0], Stmt::If { .. }));
    }

    #[test]
    fn single_line_suite() {
        let module = parse_ok("def f(): return 1\n");
        let Stmt::FuncDef(func) = &module.body[0] else {
            panic!("expected a function definition");
        };
        assert_eq!(func.body, vec![Stmt::Return(Some(Expr::Int(1)))]);
    }

    #[test]
    fn out_of_subset_constructs_are_rejected_clearly() {
        assert!(parse_src("obj.x = 5\n").is_err());
        assert!(parse_src("a, b = 1, 2\n").is_err());
        assert!(parse_src("a / b\n").is_err());
        assert!(parse_src("def f(x=1): return x\n").is_err());
        assert!(parse_src("import os\n").is_err());
        assert!(parse_src("for x in stuff:\n    pass\n").is_err());
    }
}
