//! AST node-visit counters: the DENOMINATOR that turns a wall clock into a per-node cost.

use crate::ast::{Expression, Statement};

/// One counter per [`Statement`] variant.
pub const STATEMENT_KINDS: usize = 21;

/// One counter per [`Expression`] variant.
///
/// IT IS THE ARRAY'S WIDTH AND NOT ONLY A COUNT. `expression_index` returns an index into
/// [`Counters::expressions`], so a variant added without raising this is an out-of-bounds panic
/// rather than a miscount -- and the two constants below are indexed by the same number.
pub const EXPRESSION_KINDS: usize = 28;

/// The name of each statement counter, in index order.
pub const STATEMENT_NAMES: [&str; STATEMENT_KINDS] = [
    "Expression", "Block", "Empty", "Declaration", "If", "While", "DoWhile", "For", "ForIn",
    "ForOf", "Return", "Break", "Continue", "Throw", "Try", "Labeled", "Switch", "Function",
    "Class", "Debugger", "With",
];

/// The name of each expression counter, in index order.
pub const EXPRESSION_NAMES: [&str; EXPRESSION_KINDS] = [
    "Identifier", "Number", "String", "Boolean", "Null", "This", "Template", "Tagged", "RegExp",
    "Array", "Object", "Function", "Arrow", "Class", "Super", "Unary", "Update", "Binary",
    "Logical", "Assignment", "Conditional", "Call", "New", "Member", "Sequence", "Parenthesized",
    "NewTarget", "Yield",
];

/// How many times each kind of AST node was dispatched on.
///
/// It lives on the [`Interpreter`](crate::Interpreter) rather than in a global. A global would be
/// shared by every interpreter in the process, so a harness that builds a fresh realm per program
/// -- which is exactly what the bench does, because the realm is a fixed cost that has to be timed
/// separately -- would accumulate one program's counts into the next one's report.
#[derive(Debug, Clone, Default)]
pub struct NodeCounters {
    /// Indexed by [`statement_index`].
    pub statements: [u64; STATEMENT_KINDS],
    /// Indexed by [`expression_index`].
    pub expressions: [u64; EXPRESSION_KINDS],
}

impl NodeCounters {
    /// Every statement dispatch.
    #[must_use]
    pub fn total_statements(&self) -> u64 {
        self.statements.iter().sum()
    }

    /// Every expression dispatch.
    #[must_use]
    pub fn total_expressions(&self) -> u64 {
        self.expressions.iter().sum()
    }

    /// Every dispatch of either kind -- the denominator a per-node cost divides by.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total_statements() + self.total_expressions()
    }

    /// `(name, count)` for every counter that fired, statements then expressions.
    ///
    /// Zero rows are dropped: a report listing every one of the
    /// [`STATEMENT_KINDS`] + [`EXPRESSION_KINDS`] kinds for a program that used nine of them buries
    /// the answer in blanks.
    #[must_use]
    pub fn rows(&self) -> crate::Vec<(&'static str, u64)> {
        let mut rows = crate::Vec::new();
        for (index, count) in self.statements.iter().enumerate() {
            if *count > 0 {
                rows.push((STATEMENT_NAMES[index], *count));
            }
        }
        for (index, count) in self.expressions.iter().enumerate() {
            if *count > 0 {
                rows.push((EXPRESSION_NAMES[index], *count));
            }
        }
        rows
    }
}

/// Which statement counter a node belongs to.
///
/// Written as an exhaustive `match` with no catch-all arm, ON PURPOSE. A wildcard would keep
/// compiling when a variant is added to the enum and would silently file the new kind under
/// whatever it fell through to -- the same shape as the early-error pass whose empty arm switched
/// off every rule it owned the day the parser started building `ForInit::Pattern`. This way the
/// compiler names the omission.
#[must_use]
pub fn statement_index(statement: &Statement) -> usize {
    match statement {
        Statement::Expression { .. } => 0,
        Statement::Block { .. } => 1,
        Statement::Empty { .. } => 2,
        Statement::Declaration { .. } => 3,
        Statement::If { .. } => 4,
        Statement::While { .. } => 5,
        Statement::DoWhile { .. } => 6,
        Statement::For { .. } => 7,
        Statement::ForIn { .. } => 8,
        Statement::ForOf { .. } => 9,
        Statement::Return { .. } => 10,
        Statement::Break { .. } => 11,
        Statement::Continue { .. } => 12,
        Statement::Throw { .. } => 13,
        Statement::Try { .. } => 14,
        Statement::Labeled { .. } => 15,
        Statement::Switch { .. } => 16,
        Statement::Function(_) => 17,
        Statement::Class(_) => 18,
        Statement::Debugger { .. } => 19,
        Statement::With { .. } => 20,
    }
}

/// Which expression counter a node belongs to. Exhaustive for the reason [`statement_index`] is.
#[must_use]
pub fn expression_index(expression: &Expression) -> usize {
    match expression {
        Expression::Identifier { .. } => 0,
        Expression::Number { .. } => 1,
        Expression::String { .. } => 2,
        Expression::Boolean { .. } => 3,
        Expression::Null { .. } => 4,
        Expression::This { .. } => 5,
        Expression::Template { .. } => 6,
        Expression::Tagged { .. } => 7,
        Expression::RegExp { .. } => 8,
        Expression::Array { .. } => 9,
        Expression::Object { .. } => 10,
        Expression::Function(_) => 11,
        Expression::Arrow(_) => 12,
        Expression::Class(_) => 13,
        Expression::Super { .. } => 14,
        Expression::Unary { .. } => 15,
        Expression::Update { .. } => 16,
        Expression::Binary { .. } => 17,
        Expression::Logical { .. } => 18,
        Expression::Assignment { .. } => 19,
        Expression::Conditional { .. } => 20,
        Expression::Call { .. } => 21,
        Expression::New { .. } => 22,
        Expression::Member { .. } => 23,
        Expression::Sequence { .. } => 24,
        Expression::Parenthesized { .. } => 25,
        Expression::NewTarget { .. } => 26,
        Expression::Yield { .. } => 27,
    }
}
