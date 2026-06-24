//! Lowering the AST to our bytecode.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use lamella_py_bytecode as bc;

use crate::ast::{self, Assign, BoolOp, Expr, FuncDef, ModuleAst, Stmt};

/// A failure while lowering the AST to bytecode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    /// What went wrong.
    pub message: String,
}

impl core::fmt::Display for CompileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.message)
    }
}

fn error(message: &str) -> CompileError {
    CompileError {
        message: String::from(message),
    }
}

/// Compile a module AST to a [`bc::Module`]: each top-level `def` becomes a function
/// code object, and the remaining top-level statements become the `<module>` body.
pub fn compile_module(name: &str, ast: &ModuleAst) -> Result<bc::Module, CompileError> {
    let mut functions = Vec::new();
    let mut top_level: Vec<&Stmt> = Vec::new();
    for stmt in &ast.body {
        match stmt {
            Stmt::FuncDef(func) => functions.push(compile_function(func)?),
            other => top_level.push(other),
        }
    }
    let body = compile_code_object(Scope::Module, "<module>", &[], &None, &top_level)?;
    Ok(bc::Module {
        name: String::from(name),
        functions,
        body,
    })
}

/// Whether the code object being compiled is a function body or the module's
/// top-level body. A `return` is only valid in a function.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Function,
    Module,
}

fn compile_function(func: &FuncDef) -> Result<bc::CodeObject, CompileError> {
    let body: Vec<&Stmt> = func.body.iter().collect();
    compile_code_object(Scope::Function, &func.name, &func.params, &func.ret, &body)
}

/// Resolve an annotation expression to a first-light type: a bare `int` is the typed
/// integer path; everything else (including no annotation) is dynamic.
fn resolve_type(annotation: &Option<Expr>) -> bc::StaticType {
    match annotation {
        Some(Expr::Name(name)) if name == "int" => bc::StaticType::Int,
        _ => bc::StaticType::Dynamic,
    }
}

fn compile_code_object(
    scope: Scope,
    name: &str,
    params: &[ast::ParamDef],
    ret: &Option<Expr>,
    body: &[&Stmt],
) -> Result<bc::CodeObject, CompileError> {
    let mut local_names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
    let mut local_types: Vec<bc::StaticType> =
        params.iter().map(|p| resolve_type(&p.annotation)).collect();
    collect_locals(body, &mut local_names, &mut local_types);
    infer_local_types(params, body, &local_names, &mut local_types);

    let mut compiler = Compiler {
        scope,
        asm: Assembler::new(),
        consts: Vec::new(),
        names: Vec::new(),
        local_names,
        local_types,
    };
    for stmt in body {
        compiler.compile_stmt(stmt)?;
    }
    let none = compiler.const_index(bc::Const::None);
    compiler.asm.emit(bc::Op::LoadConst(none));
    compiler.asm.emit(bc::Op::Return);

    let (ops, cache_count) = compiler.asm.finish();
    let co_params: Vec<bc::Param> = params
        .iter()
        .map(|p| bc::Param {
            name: p.name.clone(),
            ty: resolve_type(&p.annotation),
        })
        .collect();
    Ok(bc::CodeObject {
        name: String::from(name),
        params: co_params,
        ret_ty: resolve_type(ret),
        n_locals: compiler.local_names.len(),
        local_names: compiler.local_names,
        local_types: compiler.local_types,
        consts: compiler.consts,
        names: compiler.names,
        ops,
        cache_count,
    })
}

/// Collect every name a body assigns (descending into `if`/`while` bodies, since
/// Python has no block scope) into the local table, recording an annotated type
/// where one is given. Parameters are already present.
fn collect_locals(body: &[&Stmt], names: &mut Vec<String>, types: &mut Vec<bc::StaticType>) {
    for stmt in body {
        collect_locals_stmt(stmt, names, types);
    }
}

fn collect_locals_stmt(stmt: &Stmt, names: &mut Vec<String>, types: &mut Vec<bc::StaticType>) {
    match stmt {
        Stmt::Assign(Assign {
            target, annotation, ..
        }) => {
            let ty = resolve_type(annotation);
            match names.iter().position(|n| n == target) {
                None => {
                    names.push(target.clone());
                    types.push(ty);
                }
                Some(i) => {
                    if annotation.is_some() {
                        types[i] = ty;
                    }
                }
            }
        }
        Stmt::If { body, orelse, .. } => {
            for s in body {
                collect_locals_stmt(s, names, types);
            }
            for s in orelse {
                collect_locals_stmt(s, names, types);
            }
        }
        Stmt::While { body, .. } => {
            for s in body {
                collect_locals_stmt(s, names, types);
            }
        }
        Stmt::For { target, body, .. } => {
            if !names.iter().any(|n| n == target) {
                names.push(target.clone());
                types.push(bc::StaticType::Int);
            }
            for s in body {
                collect_locals_stmt(s, names, types);
            }
        }
        Stmt::Return(_) | Stmt::Expr(_) | Stmt::FuncDef(_) => {}
    }
}

/// Infer `int` for unannotated locals whose every value-assignment is statically an
/// integer (so `x = 5` needs no `: int`). An optimistic fixpoint: start each
/// unannotated slot at `Int`, then demote any whose right-hand side is not provably
/// `Int` -- to a fixed point, so a chain like `a = 0; b = a; c = obj.x; a = c` all
/// settle. Parameters and annotated locals are pinned to their declared type.
fn infer_local_types(
    params: &[ast::ParamDef],
    body: &[&Stmt],
    names: &[String],
    types: &mut [bc::StaticType],
) {
    let mut pinned = vec![false; names.len()];
    for p in pinned.iter_mut().take(params.len()) {
        *p = true;
    }
    let mut rhss: Vec<Vec<Expr>> = vec![Vec::new(); names.len()];
    for stmt in body {
        gather_assignments_stmt(stmt, names, &mut pinned, &mut rhss);
    }
    for (i, ty) in types.iter_mut().enumerate() {
        if !pinned[i] {
            *ty = bc::StaticType::Int;
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..names.len() {
            if pinned[i] || types[i] != bc::StaticType::Int {
                continue;
            }
            let provably_int = !rhss[i].is_empty()
                && rhss[i]
                    .iter()
                    .all(|e| expr_static_type(e, names, types) == bc::StaticType::Int);
            if !provably_int {
                types[i] = bc::StaticType::Dynamic;
                changed = true;
            }
        }
    }
}

/// Walk a statement, pinning the targets of annotated assignments and collecting each
/// local's value-assignment right-hand sides (for [`infer_local_types`]).
fn gather_assignments_stmt(
    stmt: &Stmt,
    names: &[String],
    pinned: &mut [bool],
    rhss: &mut [Vec<Expr>],
) {
    match stmt {
        Stmt::Assign(Assign {
            target,
            annotation,
            value,
        }) => {
            if let Some(slot) = names.iter().position(|n| n == target) {
                if annotation.is_some() {
                    pinned[slot] = true;
                }
                if let Some(v) = value {
                    rhss[slot].push(v.clone());
                }
            }
        }
        Stmt::If { body, orelse, .. } => {
            for s in body {
                gather_assignments_stmt(s, names, pinned, rhss);
            }
            for s in orelse {
                gather_assignments_stmt(s, names, pinned, rhss);
            }
        }
        Stmt::While { body, .. } => {
            for s in body {
                gather_assignments_stmt(s, names, pinned, rhss);
            }
        }
        Stmt::For { target, body, .. } => {
            if let Some(slot) = names.iter().position(|n| n == target) {
                pinned[slot] = true;
            }
            for s in body {
                gather_assignments_stmt(s, names, pinned, rhss);
            }
        }
        Stmt::Return(_) | Stmt::Expr(_) | Stmt::FuncDef(_) => {}
    }
}

/// The statically-known first-light type of an expression given the locals settled so
/// far: an integer/boolean literal, an integer-typed name, or arithmetic/comparison
/// over integers is `Int`; a call result, attribute, `None`, or string is `Dynamic`.
fn expr_static_type(expr: &Expr, names: &[String], types: &[bc::StaticType]) -> bc::StaticType {
    let both_int = |a: &Expr, b: &Expr| {
        expr_static_type(a, names, types) == bc::StaticType::Int
            && expr_static_type(b, names, types) == bc::StaticType::Int
    };
    match expr {
        Expr::Int(_) | Expr::Bool(_) => bc::StaticType::Int,
        Expr::Name(n) => names
            .iter()
            .position(|x| x == n)
            .map(|i| types[i])
            .unwrap_or(bc::StaticType::Dynamic),
        Expr::Binary { lhs, rhs, .. }
        | Expr::Compare { lhs, rhs, .. }
        | Expr::BoolBinary { lhs, rhs, .. } => {
            if both_int(lhs, rhs) {
                bc::StaticType::Int
            } else {
                bc::StaticType::Dynamic
            }
        }
        Expr::Unary { operand, .. } => expr_static_type(operand, names, types),
        Expr::Not { .. } => bc::StaticType::Int,
        Expr::Conditional { body, orelse, .. } => {
            if both_int(body, orelse) {
                bc::StaticType::Int
            } else {
                bc::StaticType::Dynamic
            }
        }
        _ => bc::StaticType::Dynamic,
    }
}

struct Compiler {
    scope: Scope,
    asm: Assembler,
    consts: Vec<bc::Const>,
    names: Vec<String>,
    local_names: Vec<String>,
    local_types: Vec<bc::StaticType>,
}

impl Compiler {
    /// Intern a constant, returning its pool index.
    fn const_index(&mut self, value: bc::Const) -> u32 {
        if let Some(i) = self.consts.iter().position(|c| *c == value) {
            return i as u32;
        }
        self.consts.push(value);
        (self.consts.len() - 1) as u32
    }

    /// Intern an attribute/global name, returning its pool index.
    fn name_index(&mut self, name: &str) -> u32 {
        if let Some(i) = self.names.iter().position(|n| n == name) {
            return i as u32;
        }
        self.names.push(String::from(name));
        (self.names.len() - 1) as u32
    }

    /// The local slot for `name`, if it is a local (or parameter).
    fn local_slot(&self, name: &str) -> Option<u32> {
        self.local_names.iter().position(|n| n == name).map(|i| i as u32)
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), CompileError> {
        match stmt {
            Stmt::FuncDef(_) => {
                Err(error("nested function definitions are out of the first-light subset"))
            }
            Stmt::Return(value) => {
                if self.scope != Scope::Function {
                    return Err(error("'return' outside a function"));
                }
                match value {
                    Some(expr) => self.compile_expr_tail(expr)?,
                    None => {
                        let none = self.const_index(bc::Const::None);
                        self.asm.emit(bc::Op::LoadConst(none));
                    }
                }
                self.asm.emit(bc::Op::Return);
                Ok(())
            }
            Stmt::Assign(assign) => self.compile_assign(assign),
            Stmt::Expr(expr) => {
                self.compile_expr_tail(expr)?;
                self.asm.emit(bc::Op::PopTop);
                Ok(())
            }
            Stmt::If { test, body, orelse } => self.compile_if(test, body, orelse),
            Stmt::While { test, body } => self.compile_while(test, body),
            Stmt::For {
                target,
                start,
                stop,
                body,
            } => self.compile_for(target, start, stop, body),
        }
    }

    fn compile_assign(&mut self, assign: &Assign) -> Result<(), CompileError> {
        let Some(value) = &assign.value else {
            return Ok(());
        };
        self.compile_expr_tail(value)?;
        let slot = self
            .local_slot(&assign.target)
            .expect("an assigned name is always a local (added by the pre-pass)");
        self.asm.emit(bc::Op::StoreFast(slot));
        Ok(())
    }

    fn compile_if(
        &mut self,
        test: &Expr,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), CompileError> {
        self.compile_expr_tail(test)?;
        let else_label = self.asm.new_label();
        self.asm.emit_branch(else_label);
        for stmt in body {
            self.compile_stmt(stmt)?;
        }
        if orelse.is_empty() {
            self.asm.place(else_label);
        } else {
            let end_label = self.asm.new_label();
            self.asm.emit_jump(end_label);
            self.asm.place(else_label);
            for stmt in orelse {
                self.compile_stmt(stmt)?;
            }
            self.asm.place(end_label);
        }
        Ok(())
    }

    fn compile_while(&mut self, test: &Expr, body: &[Stmt]) -> Result<(), CompileError> {
        let top_label = self.asm.new_label();
        self.asm.place(top_label);
        self.compile_expr_tail(test)?;
        let end_label = self.asm.new_label();
        self.asm.emit_branch(end_label);
        for stmt in body {
            self.compile_stmt(stmt)?;
        }
        self.asm.emit_jump(top_label);
        self.asm.place(end_label);
        Ok(())
    }

    /// `for target in range(start, stop): body` -- desugared to a counted loop over a
    /// hidden integer counter, so the loop variable holds the last value after the loop
    /// (as in Python). `start` and `stop` are each evaluated once into a temporary.
    fn compile_for(
        &mut self,
        target: &str,
        start: &Expr,
        stop: &Expr,
        body: &[Stmt],
    ) -> Result<(), CompileError> {
        let counter = self.alloc_temp();
        let stop_tmp = self.alloc_temp();
        self.compile_expr_tail(start)?;
        self.asm.emit(bc::Op::StoreFast(counter));
        self.compile_expr_tail(stop)?;
        self.asm.emit(bc::Op::StoreFast(stop_tmp));
        let target_slot = self
            .local_slot(target)
            .expect("the loop variable is a local (added by the pre-pass)");

        let top = self.asm.new_label();
        let end = self.asm.new_label();
        self.asm.place(top);
        self.asm.emit(bc::Op::LoadFast(counter));
        self.asm.emit(bc::Op::LoadFast(stop_tmp));
        self.asm.emit(bc::Op::Compare(bc::CmpOp::Lt));
        self.asm.emit_branch(end);
        self.asm.emit(bc::Op::LoadFast(counter));
        self.asm.emit(bc::Op::StoreFast(target_slot));
        for stmt in body {
            self.compile_stmt(stmt)?;
        }
        self.asm.emit(bc::Op::LoadFast(counter));
        let one = self.const_index(bc::Const::Int(1));
        self.asm.emit(bc::Op::LoadConst(one));
        self.asm.emit(bc::Op::Binary(bc::BinOp::Add));
        self.asm.emit(bc::Op::StoreFast(counter));
        self.asm.emit_jump(top);
        self.asm.place(end);
        Ok(())
    }

    /// Compile an expression in TAIL position -- the whole right-hand side of an
    /// assignment, an `if`/`while` test, or a `return` value -- where the operand
    /// stack is empty, so a short-circuiting boolean operator may emit its branches.
    /// Any non-boolean expression delegates to [`Self::compile_expr`].
    fn compile_expr_tail(&mut self, expr: &Expr) -> Result<(), CompileError> {
        match expr {
            Expr::BoolBinary { op, lhs, rhs } => self.compile_bool_binary(*op, lhs, rhs),
            Expr::Not { operand } => self.compile_not(operand),
            Expr::Conditional { test, body, orelse } => {
                self.compile_conditional(test, body, orelse)
            }
            _ => self.compile_expr(expr),
        }
    }

    fn compile_expr(&mut self, expr: &Expr) -> Result<(), CompileError> {
        match expr {
            Expr::Int(value) => {
                let idx = self.const_index(bc::Const::Int(*value));
                self.asm.emit(bc::Op::LoadConst(idx));
            }
            Expr::Bool(value) => {
                let idx = self.const_index(bc::Const::Bool(*value));
                self.asm.emit(bc::Op::LoadConst(idx));
            }
            Expr::None => {
                let idx = self.const_index(bc::Const::None);
                self.asm.emit(bc::Op::LoadConst(idx));
            }
            Expr::Name(name) => match self.local_slot(name) {
                Some(slot) => self.asm.emit(bc::Op::LoadFast(slot)),
                None => {
                    let idx = self.name_index(name);
                    self.asm.emit(bc::Op::LoadGlobal(idx));
                }
            },
            Expr::Attribute { value, attr } => {
                self.compile_expr(value)?;
                let name = self.name_index(attr);
                let cache = self.asm.next_cache_slot();
                self.asm.emit(bc::Op::LoadAttr { name, cache });
            }
            Expr::Binary { op, lhs, rhs } => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.asm.emit(bc::Op::Binary(binop_sel(*op)));
            }
            Expr::Unary { op, operand } => {
                self.compile_expr(operand)?;
                self.asm.emit(bc::Op::Unary(unop_sel(*op)));
            }
            Expr::BoolBinary { .. } | Expr::Not { .. } | Expr::Conditional { .. } => {
                return Err(error(
                    "a boolean operator (and/or/not) or conditional expression nested in a \
                     larger expression is out of the first-light subset; assign it to a \
                     variable first",
                ));
            }
            Expr::Compare { op, lhs, rhs } => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.asm.emit(bc::Op::Compare(cmp_sel(*op)));
            }
            Expr::Call { func, args } => {
                self.compile_expr(func)?;
                for arg in args {
                    self.compile_expr(arg)?;
                }
                self.asm.emit(bc::Op::Call(args.len() as u32));
            }
        }
        Ok(())
    }

    /// Allocate a fresh synthetic integer-typed local (a boolean short-circuit
    /// result). Its name begins with `.`, which no source identifier can, so it never
    /// collides with a user local.
    fn alloc_temp(&mut self) -> u32 {
        let slot = self.local_names.len() as u32;
        self.local_names.push(format!(".t{slot}"));
        self.local_types.push(bc::StaticType::Int);
        slot
    }

    /// `a and b` / `a or b` -- short-circuit through a temporary, so the operand stack
    /// is empty at every block boundary (the lowering's invariant). The result is one
    /// of the operand values, per Python.
    fn compile_bool_binary(
        &mut self,
        op: BoolOp,
        lhs: &Expr,
        rhs: &Expr,
    ) -> Result<(), CompileError> {
        let tmp = self.alloc_temp();
        self.compile_expr_tail(lhs)?;
        self.asm.emit(bc::Op::StoreFast(tmp));
        let end = self.asm.new_label();
        self.asm.emit(bc::Op::LoadFast(tmp));
        match op {
            BoolOp::And => {
                self.asm.emit_branch(end);
                self.compile_expr_tail(rhs)?;
                self.asm.emit(bc::Op::StoreFast(tmp));
            }
            BoolOp::Or => {
                let eval_rhs = self.asm.new_label();
                self.asm.emit_branch(eval_rhs);
                self.asm.emit_jump(end);
                self.asm.place(eval_rhs);
                self.compile_expr_tail(rhs)?;
                self.asm.emit(bc::Op::StoreFast(tmp));
            }
        }
        self.asm.place(end);
        self.asm.emit(bc::Op::LoadFast(tmp));
        Ok(())
    }

    /// `not operand` -- a boolean (`0`/`1`) from the operand's truthiness, via a
    /// temporary so the stack stays empty across the branch.
    fn compile_not(&mut self, operand: &Expr) -> Result<(), CompileError> {
        let tmp = self.alloc_temp();
        self.compile_expr_tail(operand)?;
        let falsey = self.asm.new_label();
        let end = self.asm.new_label();
        self.asm.emit_branch(falsey);
        let zero = self.const_index(bc::Const::Int(0));
        self.asm.emit(bc::Op::LoadConst(zero));
        self.asm.emit(bc::Op::StoreFast(tmp));
        self.asm.emit_jump(end);
        self.asm.place(falsey);
        let one = self.const_index(bc::Const::Int(1));
        self.asm.emit(bc::Op::LoadConst(one));
        self.asm.emit(bc::Op::StoreFast(tmp));
        self.asm.place(end);
        self.asm.emit(bc::Op::LoadFast(tmp));
        Ok(())
    }

    /// `body if test else orelse` -- branch on the test's truthiness, storing the
    /// chosen value to a temporary (so the stack stays empty across the branch).
    fn compile_conditional(
        &mut self,
        test: &Expr,
        body: &Expr,
        orelse: &Expr,
    ) -> Result<(), CompileError> {
        let tmp = self.alloc_temp();
        self.compile_expr_tail(test)?;
        let else_case = self.asm.new_label();
        let end = self.asm.new_label();
        self.asm.emit_branch(else_case);
        self.compile_expr_tail(body)?;
        self.asm.emit(bc::Op::StoreFast(tmp));
        self.asm.emit_jump(end);
        self.asm.place(else_case);
        self.compile_expr_tail(orelse)?;
        self.asm.emit(bc::Op::StoreFast(tmp));
        self.asm.place(end);
        self.asm.emit(bc::Op::LoadFast(tmp));
        Ok(())
    }
}

fn binop_sel(op: ast::BinOp) -> bc::BinOp {
    match op {
        ast::BinOp::Add => bc::BinOp::Add,
        ast::BinOp::Sub => bc::BinOp::Sub,
        ast::BinOp::Mul => bc::BinOp::Mul,
        ast::BinOp::FloorDiv => bc::BinOp::FloorDiv,
        ast::BinOp::Mod => bc::BinOp::Mod,
        ast::BinOp::BitAnd => bc::BinOp::BitAnd,
        ast::BinOp::BitOr => bc::BinOp::BitOr,
        ast::BinOp::BitXor => bc::BinOp::BitXor,
        ast::BinOp::LShift => bc::BinOp::LShift,
        ast::BinOp::RShift => bc::BinOp::RShift,
    }
}

fn unop_sel(op: ast::UnaryOp) -> bc::UnaryOp {
    match op {
        ast::UnaryOp::Neg => bc::UnaryOp::Neg,
        ast::UnaryOp::Pos => bc::UnaryOp::Pos,
        ast::UnaryOp::Invert => bc::UnaryOp::Invert,
    }
}

fn cmp_sel(op: ast::CmpOp) -> bc::CmpOp {
    match op {
        ast::CmpOp::Eq => bc::CmpOp::Eq,
        ast::CmpOp::Ne => bc::CmpOp::Ne,
        ast::CmpOp::Lt => bc::CmpOp::Lt,
        ast::CmpOp::Le => bc::CmpOp::Le,
        ast::CmpOp::Gt => bc::CmpOp::Gt,
        ast::CmpOp::Ge => bc::CmpOp::Ge,
    }
}

/// A label-based assembler over the decoded [`bc::Op`] stream. Jumps are emitted
/// against symbolic labels; on [`Assembler::finish`] each is resolved to its target's
/// absolute op index. Inline-cache slots are handed out in ascending emission order
/// (which is static order), and the total is returned as the cache count.
struct Assembler {
    emits: Vec<Emit>,
    labels: Vec<Option<u32>>,
    cache_count: u32,
}

/// One pending emission: a ready op, or a jump whose target is a not-yet-resolved
/// label.
enum Emit {
    Op(bc::Op),
    Jump(Label),
    Branch(Label),
}

#[derive(Clone, Copy)]
struct Label(usize);

impl Assembler {
    fn new() -> Self {
        Assembler {
            emits: Vec::new(),
            labels: Vec::new(),
            cache_count: 0,
        }
    }

    fn new_label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    /// Bind `label` to the next op's index.
    fn place(&mut self, label: Label) {
        self.labels[label.0] = Some(self.emits.len() as u32);
    }

    fn emit(&mut self, op: bc::Op) {
        self.emits.push(Emit::Op(op));
    }

    fn emit_jump(&mut self, target: Label) {
        self.emits.push(Emit::Jump(target));
    }

    fn emit_branch(&mut self, target: Label) {
        self.emits.push(Emit::Branch(target));
    }

    /// Hand out the next inline-cache slot (ascending static order).
    fn next_cache_slot(&mut self) -> u32 {
        let slot = self.cache_count;
        self.cache_count += 1;
        slot
    }

    /// Resolve jump targets and produce the op stream plus the inline-cache count.
    fn finish(self) -> (Vec<bc::Op>, usize) {
        let mut ops = Vec::with_capacity(self.emits.len());
        for emit in &self.emits {
            let op = match emit {
                Emit::Op(op) => *op,
                Emit::Jump(label) => {
                    bc::Op::Jump(self.labels[label.0].expect("every jump label is placed"))
                }
                Emit::Branch(label) => bc::Op::PopJumpIfFalse(
                    self.labels[label.0].expect("every branch label is placed"),
                ),
            };
            ops.push(op);
        }
        (ops, self.cache_count as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;
    use crate::parser::parse;
    use bc::{BinOp, Const, Op, StaticType};

    fn compile_src(source: &str) -> Result<bc::Module, CompileError> {
        let ast = parse(tokenize(source).expect("tokenizes")).expect("parses");
        compile_module("test", &ast)
    }

    fn func<'a>(module: &'a bc::Module, name: &str) -> &'a bc::CodeObject {
        module
            .functions
            .iter()
            .find(|f| f.name == name)
            .expect("function present")
    }

    #[test]
    fn unannotated_int_locals_are_inferred() {
        let module = compile_src("def f() -> int:\n    x = 5\n    y = x + 1\n    return y\n").unwrap();
        assert_eq!(
            func(&module, "f").local_types,
            vec![StaticType::Int, StaticType::Int]
        );
    }

    #[test]
    fn a_dynamic_assignment_keeps_the_local_dynamic() {
        let module = compile_src("def f(obj) -> int:\n    x = obj.y\n    return 0\n").unwrap();
        assert_eq!(
            func(&module, "f").local_types,
            vec![StaticType::Dynamic, StaticType::Dynamic]
        );
    }

    #[test]
    fn a_mixed_local_demotes_to_dynamic() {
        let module =
            compile_src("def f(obj) -> int:\n    x = 0\n    x = obj.y\n    return 0\n").unwrap();
        assert_eq!(func(&module, "f").local_types[1], StaticType::Dynamic);
    }

    #[test]
    fn whole_expression_booleans_compile() {
        assert!(
            compile_src("def f(a: int, b: int) -> int:\n    x = a and b\n    return x\n").is_ok()
        );
        assert!(compile_src("def f(a: int) -> int:\n    x = not a\n    return x\n").is_ok());
    }

    #[test]
    fn a_nested_boolean_operator_is_rejected() {
        assert!(
            compile_src("def f(a: int, b: int) -> int:\n    return (a and b) + 1\n").is_err()
        );
    }

    #[test]
    fn a_conditional_expression_compiles_in_tail_position() {
        assert!(compile_src(
            "def f(a: int, b: int) -> int:\n    x = a if a > b else b\n    return x\n"
        )
        .is_ok());
    }

    #[test]
    fn a_nested_conditional_is_rejected() {
        assert!(compile_src("def f(a: int) -> int:\n    return 1 + (a if a else 0)\n").is_err());
    }

    #[test]
    fn a_for_loop_over_range_makes_the_variable_an_int() {
        let module = compile_src(
            "def f() -> int:\n    s = 0\n    for i in range(3):\n        s += i\n    return s\n",
        )
        .unwrap();
        let f = func(&module, "f");
        let i_slot = f.local_names.iter().position(|n| n == "i").unwrap();
        assert_eq!(f.local_types[i_slot], StaticType::Int);
    }

    #[test]
    fn typed_function_emits_integer_opcodes() {
        let module = compile_src("def inc(n: int) -> int:\n    return n + 1\n").unwrap();
        let inc = func(&module, "inc");
        assert_eq!(inc.params.len(), 1);
        assert_eq!(inc.params[0].ty, StaticType::Int);
        assert_eq!(inc.ret_ty, StaticType::Int);
        assert_eq!(inc.local_types, vec![StaticType::Int]);
        assert_eq!(inc.cache_count, 0);
        assert_eq!(
            inc.ops,
            vec![
                Op::LoadFast(0),
                Op::LoadConst(0),
                Op::Binary(BinOp::Add),
                Op::Return,
                Op::LoadConst(1),
                Op::Return,
            ]
        );
        assert_eq!(inc.consts, vec![Const::Int(1), Const::None]);
    }

    #[test]
    fn attribute_access_is_one_dynamic_site() {
        let module = compile_src("def get_x(obj):\n    return obj.x\n").unwrap();
        let get_x = func(&module, "get_x");
        assert_eq!(get_x.local_types, vec![StaticType::Dynamic]);
        assert_eq!(get_x.cache_count, 1);
        assert_eq!(get_x.names, vec![String::from("x")]);
        assert_eq!(
            get_x.ops,
            vec![
                Op::LoadFast(0),
                Op::LoadAttr { name: 0, cache: 0 },
                Op::Return,
                Op::LoadConst(0),
                Op::Return,
            ]
        );
    }

    #[test]
    fn while_loop_jumps_resolve_to_op_indices() {
        let module =
            compile_src("def f(n: int) -> int:\n    while n > 0:\n        n = n - 1\n    return n\n")
                .unwrap();
        let f = func(&module, "f");
        let back = f
            .ops
            .iter()
            .find_map(|op| match op {
                Op::Jump(t) => Some(*t),
                _ => None,
            })
            .expect("a back-edge");
        let exit = f
            .ops
            .iter()
            .find_map(|op| match op {
                Op::PopJumpIfFalse(t) => Some(*t),
                _ => None,
            })
            .expect("a conditional exit");
        assert_eq!(back, 0);
        assert!((exit as usize) < f.ops.len());
        assert!(exit > 0);
    }

    #[test]
    fn return_outside_a_function_is_rejected() {
        assert!(compile_src("return 1\n").is_err());
    }

    #[test]
    fn if_else_has_two_jumps() {
        let module = compile_src(
            "def f(n: int) -> int:\n    if n > 0:\n        return 1\n    else:\n        return 2\n",
        )
        .unwrap();
        let f = func(&module, "f");
        assert_eq!(
            f.ops
                .iter()
                .filter(|op| matches!(op, Op::PopJumpIfFalse(_)))
                .count(),
            1
        );
        assert_eq!(
            f.ops.iter().filter(|op| matches!(op, Op::Jump(_))).count(),
            1
        );
    }
}
