//! Lowering the AST to our bytecode.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use lamella_py_bytecode as bc;

use crate::ast::{self, Assign, BoolOp, CompClause, ExceptHandler, Expr, FuncDef, ModuleAst, Stmt};

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
            Stmt::ClassDef { name, body, .. } => {
                for member in body {
                    if let Stmt::FuncDef(method) = member {
                        functions.push(compile_method(name, method)?);
                    }
                }
                top_level.push(stmt);
            }
            other => top_level.push(other),
        }
    }
    let body = compile_code_object(Scope::Module, "<module>", &[], &None, &top_level, None)?;
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
    compile_code_object(Scope::Function, &func.name, &func.params, &func.ret, &body, None)
}

/// Compile a class method as a Module function named `"ClassName.method"`; the class body
/// emits `MakeFunction` referencing it by that qualified name.
fn compile_method(class_name: &str, method: &FuncDef) -> Result<bc::CodeObject, CompileError> {
    let mut qualified = String::from(class_name);
    qualified.push('.');
    qualified.push_str(&method.name);
    let body: Vec<&Stmt> = method.body.iter().collect();
    compile_code_object(
        Scope::Function,
        &qualified,
        &method.params,
        &method.ret,
        &body,
        Some(class_name),
    )
}

/// Resolve an annotation expression to a static type: a bare `int` is the typed
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
    current_class: Option<&str>,
) -> Result<bc::CodeObject, CompileError> {
    let mut local_names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
    let mut local_types: Vec<bc::StaticType> =
        params.iter().map(|p| resolve_type(&p.annotation)).collect();
    collect_locals(body, &mut local_names, &mut local_types);
    for stmt in body {
        collect_comp_targets_stmt(stmt, &mut local_names, &mut local_types);
    }
    infer_local_types(params, body, &local_names, &mut local_types);

    let mut compiler = Compiler {
        scope,
        asm: Assembler::new(),
        consts: Vec::new(),
        names: Vec::new(),
        local_names,
        local_types,
        loops: Vec::new(),
        finallys: Vec::new(),
        current_class: current_class.map(String::from),
    };
    for stmt in body {
        compiler.compile_stmt(stmt)?;
    }
    let none = compiler.const_index(bc::Const::None);
    compiler.asm.emit(bc::Op::LoadConst(none));
    compiler.asm.emit(bc::Op::Return);

    let (ops, cache_count, exc_table) = compiler.asm.finish();
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
        exc_table,
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
        Stmt::MultiAssign { targets, .. } | Stmt::TupleAssign { targets, .. } => {
            for target in targets {
                if !names.iter().any(|n| n == target) {
                    names.push(target.clone());
                    types.push(bc::StaticType::Dynamic);
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
        Stmt::While { body, orelse, .. } => {
            for s in body.iter().chain(orelse) {
                collect_locals_stmt(s, names, types);
            }
        }
        Stmt::For {
            target,
            body,
            orelse,
            ..
        } => {
            if !names.iter().any(|n| n == target) {
                names.push(target.clone());
                types.push(bc::StaticType::Int);
            }
            for s in body.iter().chain(orelse) {
                collect_locals_stmt(s, names, types);
            }
        }
        Stmt::ForIter {
            target,
            body,
            orelse,
            ..
        } => {
            if !names.iter().any(|n| n == target) {
                names.push(target.clone());
                types.push(bc::StaticType::Dynamic);
            }
            for s in body.iter().chain(orelse) {
                collect_locals_stmt(s, names, types);
            }
        }
        Stmt::Try {
            body,
            handlers,
            orelse,
            finalbody,
        } => {
            for h in handlers {
                if let Some(name) = &h.name {
                    if !names.iter().any(|n| n == name) {
                        names.push(name.clone());
                        types.push(bc::StaticType::Dynamic);
                    }
                }
            }
            for s in body.iter().chain(orelse).chain(finalbody) {
                collect_locals_stmt(s, names, types);
            }
            for h in handlers {
                for s in &h.body {
                    collect_locals_stmt(s, names, types);
                }
            }
        }
        Stmt::ClassDef { name, .. } => {
            if !names.iter().any(|n| n == name) {
                names.push(name.clone());
                types.push(bc::StaticType::Dynamic);
            }
        }
        Stmt::Return(_)
        | Stmt::Expr(_)
        | Stmt::FuncDef(_)
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Pass
        | Stmt::SetItem { .. }
        | Stmt::SetAttr { .. }
        | Stmt::Raise { .. } => {}
    }
}

/// Add `name` as a dynamic local if not already present.
fn add_dynamic_local(name: &str, names: &mut Vec<String>, types: &mut Vec<bc::StaticType>) {
    if !names.iter().any(|n| n == name) {
        names.push(String::from(name));
        types.push(bc::StaticType::Dynamic);
    }
}

/// Collect the dynamic loop targets of a comprehension's clauses, descending into each
/// clause's iterable and filters for nested comprehensions.
fn collect_comp_clauses(
    clauses: &[CompClause],
    names: &mut Vec<String>,
    types: &mut Vec<bc::StaticType>,
) {
    for clause in clauses {
        for t in &clause.targets {
            add_dynamic_local(t, names, types);
        }
        collect_comp_targets_expr(&clause.iterable, names, types);
        for cond in &clause.conditions {
            collect_comp_targets_expr(cond, names, types);
        }
    }
}

/// Collect comprehension loop variables (which the emission binds + reads by name) from a
/// statement's expressions and nested bodies, as dynamic locals.
fn collect_comp_targets_stmt(
    stmt: &Stmt,
    names: &mut Vec<String>,
    types: &mut Vec<bc::StaticType>,
) {
    match stmt {
        Stmt::Assign(a) => {
            if let Some(v) = &a.value {
                collect_comp_targets_expr(v, names, types);
            }
        }
        Stmt::MultiAssign { value, .. } | Stmt::TupleAssign { value, .. } => {
            collect_comp_targets_expr(value, names, types)
        }
        Stmt::Return(Some(e)) | Stmt::Expr(e) => collect_comp_targets_expr(e, names, types),
        Stmt::SetItem {
            container,
            index,
            value,
        } => {
            collect_comp_targets_expr(container, names, types);
            collect_comp_targets_expr(index, names, types);
            collect_comp_targets_expr(value, names, types);
        }
        Stmt::SetAttr { obj, value, .. } => {
            collect_comp_targets_expr(obj, names, types);
            collect_comp_targets_expr(value, names, types);
        }
        Stmt::Raise { exc, cause } => {
            if let Some(e) = exc {
                collect_comp_targets_expr(e, names, types);
            }
            if let Some(c) = cause {
                collect_comp_targets_expr(c, names, types);
            }
        }
        Stmt::If { test, body, orelse } | Stmt::While { test, body, orelse } => {
            collect_comp_targets_expr(test, names, types);
            for s in body.iter().chain(orelse) {
                collect_comp_targets_stmt(s, names, types);
            }
        }
        Stmt::For {
            start, stop, body, orelse, ..
        } => {
            collect_comp_targets_expr(start, names, types);
            collect_comp_targets_expr(stop, names, types);
            for s in body.iter().chain(orelse) {
                collect_comp_targets_stmt(s, names, types);
            }
        }
        Stmt::ForIter {
            iterable, body, orelse, ..
        } => {
            collect_comp_targets_expr(iterable, names, types);
            for s in body.iter().chain(orelse) {
                collect_comp_targets_stmt(s, names, types);
            }
        }
        Stmt::Try {
            body, handlers, orelse, finalbody,
        } => {
            for s in body.iter().chain(orelse).chain(finalbody) {
                collect_comp_targets_stmt(s, names, types);
            }
            for h in handlers {
                for s in &h.body {
                    collect_comp_targets_stmt(s, names, types);
                }
            }
        }
        Stmt::Return(None)
        | Stmt::FuncDef(_)
        | Stmt::ClassDef { .. }
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Pass => {}
    }
}

/// Walk an expression, collecting comprehension loop variables (recursing into nested
/// comprehensions) as dynamic locals.
fn collect_comp_targets_expr(
    expr: &Expr,
    names: &mut Vec<String>,
    types: &mut Vec<bc::StaticType>,
) {
    match expr {
        Expr::ListComp { element, clauses } | Expr::SetComp { element, clauses } => {
            collect_comp_clauses(clauses, names, types);
            collect_comp_targets_expr(element, names, types);
        }
        Expr::DictComp { key, value, clauses } => {
            collect_comp_clauses(clauses, names, types);
            collect_comp_targets_expr(key, names, types);
            collect_comp_targets_expr(value, names, types);
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::Compare { lhs, rhs, .. }
        | Expr::BoolBinary { lhs, rhs, .. } => {
            collect_comp_targets_expr(lhs, names, types);
            collect_comp_targets_expr(rhs, names, types);
        }
        Expr::Unary { operand, .. } | Expr::Not { operand } => {
            collect_comp_targets_expr(operand, names, types)
        }
        Expr::Conditional { test, body, orelse } => {
            collect_comp_targets_expr(test, names, types);
            collect_comp_targets_expr(body, names, types);
            collect_comp_targets_expr(orelse, names, types);
        }
        Expr::Call { func, args } => {
            collect_comp_targets_expr(func, names, types);
            for a in args {
                collect_comp_targets_expr(a, names, types);
            }
        }
        Expr::List(es) | Expr::Tuple(es) | Expr::Set(es) => {
            for e in es {
                collect_comp_targets_expr(e, names, types);
            }
        }
        Expr::Dict(ps) => {
            for (k, v) in ps {
                collect_comp_targets_expr(k, names, types);
                collect_comp_targets_expr(v, names, types);
            }
        }
        Expr::Subscript { value, index } => {
            collect_comp_targets_expr(value, names, types);
            collect_comp_targets_expr(index, names, types);
        }
        Expr::Attribute { value, .. } => collect_comp_targets_expr(value, names, types),
        Expr::Slice { lower, upper, step } => {
            for e in [lower, upper, step].into_iter().flatten() {
                collect_comp_targets_expr(e, names, types);
            }
        }
        Expr::Int(_) | Expr::Str(_) | Expr::Bool(_) | Expr::None | Expr::Name(_) => {}
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
        Stmt::MultiAssign { targets, value } => {
            for target in targets {
                if let Some(slot) = names.iter().position(|n| n == target) {
                    rhss[slot].push(value.clone());
                }
            }
        }
        Stmt::TupleAssign { targets, .. } => {
            for target in targets {
                if let Some(slot) = names.iter().position(|n| n == target) {
                    pinned[slot] = true;
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
        Stmt::While { body, orelse, .. } => {
            for s in body.iter().chain(orelse) {
                gather_assignments_stmt(s, names, pinned, rhss);
            }
        }
        Stmt::For {
            target,
            body,
            orelse,
            ..
        } => {
            if let Some(slot) = names.iter().position(|n| n == target) {
                pinned[slot] = true;
            }
            for s in body.iter().chain(orelse) {
                gather_assignments_stmt(s, names, pinned, rhss);
            }
        }
        Stmt::ForIter {
            target,
            body,
            orelse,
            ..
        } => {
            if let Some(slot) = names.iter().position(|n| n == target) {
                pinned[slot] = true;
            }
            for s in body.iter().chain(orelse) {
                gather_assignments_stmt(s, names, pinned, rhss);
            }
        }
        Stmt::Try {
            body,
            handlers,
            orelse,
            finalbody,
        } => {
            for h in handlers {
                if let Some(name) = &h.name {
                    if let Some(slot) = names.iter().position(|n| n == name) {
                        pinned[slot] = true;
                    }
                }
            }
            for s in body.iter().chain(orelse).chain(finalbody) {
                gather_assignments_stmt(s, names, pinned, rhss);
            }
            for h in handlers {
                for s in &h.body {
                    gather_assignments_stmt(s, names, pinned, rhss);
                }
            }
        }
        Stmt::ClassDef { name, .. } => {
            if let Some(slot) = names.iter().position(|n| n == name) {
                pinned[slot] = true;
            }
        }
        Stmt::Return(_)
        | Stmt::Expr(_)
        | Stmt::FuncDef(_)
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Pass
        | Stmt::SetItem { .. }
        | Stmt::SetAttr { .. }
        | Stmt::Raise { .. } => {}
    }
}

/// The statically-known type of an expression given the locals settled so far:
/// an integer/boolean literal, an integer-typed name, or arithmetic/comparison
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
        Expr::Call { func, args }
            if matches!(&**func, Expr::Name(n) if matches!(n.as_str(), "abs" | "min" | "max"))
                && args
                    .iter()
                    .all(|a| expr_static_type(a, names, types) == bc::StaticType::Int) =>
        {
            bc::StaticType::Int
        }
        _ => bc::StaticType::Dynamic,
    }
}

/// Which container a comprehension builds, carrying the borrowed element (or key/value) to
/// emit at the innermost clause.
#[derive(Clone, Copy)]
enum CompKind<'a> {
    List(&'a Expr),
    Set(&'a Expr),
    Dict(&'a Expr, &'a Expr),
}

struct Compiler {
    scope: Scope,
    asm: Assembler,
    consts: Vec<bc::Const>,
    names: Vec<String>,
    local_names: Vec<String>,
    local_types: Vec<bc::StaticType>,
    /// A stack of the enclosing loops' `(continue, break, finally_depth)`: the jump targets
    /// plus `self.finallys.len()` at loop entry, so a break/continue re-emits only the
    /// `finally` bodies entered inside that loop.
    loops: Vec<(Label, Label, usize)>,
    /// A stack of active `finally` bodies (innermost last). An exit -- fall-through, return,
    /// break, continue, or the exception copy -- re-emits the crossed bodies (the duplication
    /// model). The bodies are stack-neutral, so a held return value survives across them.
    finallys: Vec<Vec<Stmt>>,
    /// The enclosing class name, so `super()` in a method resolves to its base.
    current_class: Option<String>,
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
                Err(error("nested function definitions are not supported in this subset"))
            }
            Stmt::Return(value) => {
                if self.scope != Scope::Function {
                    return Err(error("'return' outside a function"));
                }
                match value {
                    Some(expr) => self.compile_expr(expr)?,
                    None => {
                        let none = self.const_index(bc::Const::None);
                        self.asm.emit(bc::Op::LoadConst(none));
                    }
                }
                self.emit_finallys_from(0)?;
                self.asm.emit(bc::Op::Return);
                Ok(())
            }
            Stmt::Assign(assign) => self.compile_assign(assign),
            Stmt::MultiAssign { targets, value } => self.compile_multi_assign(targets, value),
            Stmt::TupleAssign { targets, value } => {
                self.compile_expr(value)?;
                self.asm.emit(bc::Op::UnpackSequence(targets.len() as u32));
                for target in targets {
                    let slot = self
                        .local_slot(target)
                        .expect("the tuple-unpacking target is a local (added by the pre-pass)");
                    self.asm.emit(bc::Op::StoreFast(slot));
                }
                Ok(())
            }
            Stmt::SetItem {
                container,
                index,
                value,
            } => {
                self.compile_expr(value)?;
                self.compile_expr(container)?;
                self.compile_expr(index)?;
                self.asm.emit(bc::Op::Setitem);
                Ok(())
            }
            Stmt::SetAttr { obj, attr, value } => {
                self.compile_expr(value)?;
                self.compile_expr(obj)?;
                let name = self.name_index(attr);
                let cache = self.asm.next_cache_slot();
                self.asm.emit(bc::Op::SetAttr { name, cache });
                Ok(())
            }
            Stmt::ClassDef { name, base, body } => self.compile_classdef(name, base, body),
            Stmt::Expr(expr) => {
                self.compile_expr(expr)?;
                self.asm.emit(bc::Op::PopTop);
                Ok(())
            }
            Stmt::If { test, body, orelse } => self.compile_if(test, body, orelse),
            Stmt::While { test, body, orelse } => self.compile_while(test, body, orelse),
            Stmt::For {
                target,
                start,
                stop,
                step,
                body,
                orelse,
            } => self.compile_for(target, start, stop, *step, body, orelse),
            Stmt::ForIter {
                target,
                iterable,
                body,
                orelse,
            } => self.compile_for_iter(target, iterable, body, orelse),
            Stmt::Raise { exc, cause } => {
                match (exc, cause) {
                    (Some(e), Some(c)) => {
                        self.compile_expr(e)?;
                        self.compile_expr(c)?;
                        self.asm.emit(bc::Op::Raise(2));
                    }
                    (Some(e), None) => {
                        self.compile_expr(e)?;
                        self.asm.emit(bc::Op::Raise(1));
                    }
                    (None, _) => self.asm.emit(bc::Op::Raise(0)),
                }
                Ok(())
            }
            Stmt::Try {
                body,
                handlers,
                orelse,
                finalbody,
            } => {
                if finalbody.is_empty() {
                    self.compile_try_except(body, handlers, orelse)
                } else {
                    self.compile_try_finally(body, handlers, orelse, finalbody)
                }
            }
            Stmt::Break => {
                let (_, target, depth) = self
                    .loops
                    .last()
                    .copied()
                    .ok_or_else(|| error("'break' outside a loop"))?;
                self.emit_finallys_from(depth)?;
                self.asm.emit_jump(target);
                Ok(())
            }
            Stmt::Continue => {
                let (target, _, depth) = self
                    .loops
                    .last()
                    .copied()
                    .ok_or_else(|| error("'continue' outside a loop"))?;
                self.emit_finallys_from(depth)?;
                self.asm.emit_jump(target);
                Ok(())
            }
            Stmt::Pass => Ok(()),
        }
    }

    fn compile_assign(&mut self, assign: &Assign) -> Result<(), CompileError> {
        let Some(value) = &assign.value else {
            return Ok(());
        };
        self.compile_expr(value)?;
        let slot = self
            .local_slot(&assign.target)
            .expect("an assigned name is always a local (added by the pre-pass)");
        self.asm.emit(bc::Op::StoreFast(slot));
        Ok(())
    }

    /// `a = b = value`: evaluate the value once, store it to the first target, then copy
    /// that into each remaining target (left to right), so all bind the same value.
    fn compile_multi_assign(
        &mut self,
        targets: &[String],
        value: &Expr,
    ) -> Result<(), CompileError> {
        self.compile_expr(value)?;
        let first = self
            .local_slot(&targets[0])
            .expect("an assigned name is always a local (added by the pre-pass)");
        self.asm.emit(bc::Op::StoreFast(first));
        for target in &targets[1..] {
            let slot = self
                .local_slot(target)
                .expect("an assigned name is always a local (added by the pre-pass)");
            self.asm.emit(bc::Op::LoadFast(first));
            self.asm.emit(bc::Op::StoreFast(slot));
        }
        Ok(())
    }

    fn compile_if(
        &mut self,
        test: &Expr,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), CompileError> {
        self.compile_expr(test)?;
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

    fn compile_while(
        &mut self,
        test: &Expr,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), CompileError> {
        let top_label = self.asm.new_label();
        let else_label = self.asm.new_label();
        let after_label = self.asm.new_label();
        self.asm.place(top_label);
        self.compile_expr(test)?;
        self.asm.emit_branch(else_label);
        self.loops.push((top_label, after_label, self.finallys.len()));
        for stmt in body {
            self.compile_stmt(stmt)?;
        }
        self.loops.pop();
        self.asm.emit_jump(top_label);
        self.asm.place(else_label);
        for stmt in orelse {
            self.compile_stmt(stmt)?;
        }
        self.asm.place(after_label);
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
        step: i64,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), CompileError> {
        let counter = self.alloc_temp();
        let stop_tmp = self.alloc_temp();
        self.compile_expr(start)?;
        self.asm.emit(bc::Op::StoreFast(counter));
        self.compile_expr(stop)?;
        self.asm.emit(bc::Op::StoreFast(stop_tmp));
        let target_slot = self
            .local_slot(target)
            .expect("the loop variable is a local (added by the pre-pass)");

        let top = self.asm.new_label();
        let cont = self.asm.new_label();
        let else_label = self.asm.new_label();
        let after = self.asm.new_label();
        self.asm.place(top);
        self.asm.emit(bc::Op::LoadFast(counter));
        self.asm.emit(bc::Op::LoadFast(stop_tmp));
        let cmp = if step > 0 {
            bc::CmpOp::Lt
        } else {
            bc::CmpOp::Gt
        };
        self.asm.emit(bc::Op::Compare(cmp));
        self.asm.emit_branch(else_label);
        self.asm.emit(bc::Op::LoadFast(counter));
        self.asm.emit(bc::Op::StoreFast(target_slot));
        self.loops.push((cont, after, self.finallys.len()));
        for stmt in body {
            self.compile_stmt(stmt)?;
        }
        self.loops.pop();
        self.asm.place(cont);
        self.asm.emit(bc::Op::LoadFast(counter));
        let step_const = self.const_index(bc::Const::Int(step));
        self.asm.emit(bc::Op::LoadConst(step_const));
        self.asm.emit(bc::Op::Binary(bc::BinOp::Add));
        self.asm.emit(bc::Op::StoreFast(counter));
        self.asm.emit_jump(top);
        self.asm.place(else_label);
        for stmt in orelse {
            self.compile_stmt(stmt)?;
        }
        self.asm.place(after);
        Ok(())
    }

    /// `try` / `except` / `else` (the caller rejects a `finally`). The body is plain ops
    /// covered by an `exc_table` entry; on a raise the runtime truncates the value stack and
    /// jumps to the handler chain, where each clause type-tests via `MatchExc` and binds
    /// `as name` via `LoadExc`. A chain that matches nothing `Reraise`s; `else` runs after a
    /// clean body (outside the protected range).
    fn compile_try_except(
        &mut self,
        body: &[Stmt],
        handlers: &[ExceptHandler],
        orelse: &[Stmt],
    ) -> Result<(), CompileError> {
        let body_start = self.asm.new_label();
        let body_end = self.asm.new_label();
        let handler_start = self.asm.new_label();
        let after = self.asm.new_label();
        self.asm.place(body_start);
        for stmt in body {
            self.compile_stmt(stmt)?;
        }
        self.asm.place(body_end);
        self.asm.add_exc_entry(body_start, body_end, handler_start, 0);
        for stmt in orelse {
            self.compile_stmt(stmt)?;
        }
        self.asm.emit_jump(after);
        self.asm.place(handler_start);
        for handler in handlers {
            let next = self.asm.new_label();
            if let Some(typ) = &handler.typ {
                self.compile_expr(typ)?;
                self.asm.emit(bc::Op::MatchExc);
                self.asm.emit_branch(next);
            }
            if let Some(name) = &handler.name {
                self.asm.emit(bc::Op::LoadExc);
                let slot = self
                    .local_slot(name)
                    .expect("the except-clause name is a local");
                self.asm.emit(bc::Op::StoreFast(slot));
            }
            self.asm.emit(bc::Op::PopExcept);
            for stmt in &handler.body {
                self.compile_stmt(stmt)?;
            }
            if let Some(name) = &handler.name {
                let slot = self
                    .local_slot(name)
                    .expect("the except-clause name is a local");
                self.asm.emit(bc::Op::DeleteFast(slot));
            }
            self.asm.emit_jump(after);
            self.asm.place(next);
        }
        self.asm.emit(bc::Op::Reraise);
        self.asm.place(after);
        Ok(())
    }

    /// Re-emit the active finally bodies `self.finallys[from..]` innermost-first (top-down),
    /// for an exit that crosses them. Bodies are cloned (they run several times in the
    /// duplication model) and are stack-neutral, so a held return value survives across them.
    fn emit_finallys_from(&mut self, from: usize) -> Result<(), CompileError> {
        let len = self.finallys.len();
        for depth in (from..len).rev() {
            let body = self.finallys[depth].clone();
            let saved = self.finallys.split_off(depth);
            for stmt in &body {
                self.compile_stmt(stmt)?;
            }
            self.finallys.extend(saved);
        }
        Ok(())
    }

    /// `try B [except ...] [else O] finally F` -- the duplication model (no new op). The
    /// protected body (B, or the inner try/except/else) is covered by an `exc_table` entry to
    /// a finally COPY that runs F then `Reraise`; the fall-through runs F inline; and a
    /// return/break/continue inside re-emits F via the finally-stack.
    fn compile_try_finally(
        &mut self,
        body: &[Stmt],
        handlers: &[ExceptHandler],
        orelse: &[Stmt],
        finalbody: &[Stmt],
    ) -> Result<(), CompileError> {
        let protected_start = self.asm.new_label();
        let protected_end = self.asm.new_label();
        let fcopy = self.asm.new_label();
        let after = self.asm.new_label();
        self.finallys.push(finalbody.to_vec());
        self.asm.place(protected_start);
        if handlers.is_empty() {
            for stmt in body {
                self.compile_stmt(stmt)?;
            }
        } else {
            self.compile_try_except(body, handlers, orelse)?;
        }
        self.asm.place(protected_end);
        self.finallys.pop();
        for stmt in finalbody {
            self.compile_stmt(stmt)?;
        }
        self.asm.emit_jump(after);
        self.asm.place(fcopy);
        for stmt in finalbody {
            self.compile_stmt(stmt)?;
        }
        self.asm.emit(bc::Op::Reraise);
        self.asm.add_exc_entry(protected_start, protected_end, fcopy, 0);
        self.asm.place(after);
        Ok(())
    }

    /// `class Name [(Base)]:` -- push the name and base, build the namespace dict (class
    /// attributes + a `MakeFunction("Name.method")` for each method), `BuildClass` over
    /// `[name, base, namespace]`, and bind the class object to its name.
    fn compile_classdef(
        &mut self,
        name: &str,
        base: &Option<Expr>,
        body: &[Stmt],
    ) -> Result<(), CompileError> {
        if self.scope != Scope::Module {
            return Err(error("nested class definitions are out of the subset"));
        }
        let name_const = self.const_index(bc::Const::Str(String::from(name)));
        self.asm.emit(bc::Op::LoadConst(name_const));
        match base {
            Some(b) => self.compile_expr(b)?,
            None => {
                let none = self.const_index(bc::Const::None);
                self.asm.emit(bc::Op::LoadConst(none));
            }
        }
        let mut n_members = 0u32;
        for member in body {
            match member {
                Stmt::FuncDef(method) => {
                    let key = self.const_index(bc::Const::Str(method.name.clone()));
                    self.asm.emit(bc::Op::LoadConst(key));
                    let mut qualified = String::from(name);
                    qualified.push('.');
                    qualified.push_str(&method.name);
                    let f = self.name_index(&qualified);
                    self.asm.emit(bc::Op::MakeFunction(f));
                    n_members += 1;
                }
                Stmt::Assign(assign) => {
                    if let Some(value) = &assign.value {
                        let key = self.const_index(bc::Const::Str(assign.target.clone()));
                        self.asm.emit(bc::Op::LoadConst(key));
                        self.compile_expr(value)?;
                        n_members += 1;
                    }
                }
                _ => {}
            }
        }
        self.asm.emit(bc::Op::BuildDict(n_members));
        self.asm.emit(bc::Op::BuildClass);
        let slot = self
            .local_slot(name)
            .expect("the class name is a local (added by the pre-pass)");
        self.asm.emit(bc::Op::StoreFast(slot));
        Ok(())
    }

    /// `for target in <iterable>:` over a general iterable -- the iterator protocol. The
    /// iterable is iter()'d into a temp; each pass loads it and `ForIter` either pushes the
    /// next item or, on exhaustion, jumps to the `else` clause. `break` jumps past the else.
    fn compile_for_iter(
        &mut self,
        target: &str,
        iterable: &Expr,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), CompileError> {
        self.compile_expr(iterable)?;
        self.asm.emit(bc::Op::GetIter);
        let iter_slot = self.alloc_temp();
        self.asm.emit(bc::Op::StoreFast(iter_slot));
        let top = self.asm.new_label();
        let else_label = self.asm.new_label();
        let after = self.asm.new_label();
        self.asm.place(top);
        self.asm.emit(bc::Op::LoadFast(iter_slot));
        self.asm.emit_for_iter(else_label);
        let target_slot = self
            .local_slot(target)
            .expect("the loop variable is a local (added by the pre-pass)");
        self.asm.emit(bc::Op::StoreFast(target_slot));
        self.loops.push((top, after, self.finallys.len()));
        for stmt in body {
            self.compile_stmt(stmt)?;
        }
        self.loops.pop();
        self.asm.emit_jump(top);
        self.asm.place(else_label);
        for stmt in orelse {
            self.compile_stmt(stmt)?;
        }
        self.asm.place(after);
        Ok(())
    }

    fn compile_expr(&mut self, expr: &Expr) -> Result<(), CompileError> {
        match expr {
            Expr::Int(value) => {
                let idx = self.const_index(bc::Const::Int(*value));
                self.asm.emit(bc::Op::LoadConst(idx));
            }
            Expr::Str(value) => {
                let idx = self.const_index(bc::Const::Str(value.clone()));
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
            Expr::Subscript { value, index } => {
                self.compile_expr(value)?;
                self.compile_expr(index)?;
                let cache = self.asm.next_cache_slot();
                self.asm.emit(bc::Op::Subscript { cache });
            }
            Expr::Slice { lower, upper, step } => {
                self.compile_slice_bound(lower)?;
                self.compile_slice_bound(upper)?;
                self.compile_slice_bound(step)?;
                self.asm.emit(bc::Op::BuildSlice);
            }
            Expr::List(elements) => {
                for e in elements {
                    self.compile_expr(e)?;
                }
                self.asm.emit(bc::Op::BuildList(elements.len() as u32));
            }
            Expr::Tuple(elements) => {
                for e in elements {
                    self.compile_expr(e)?;
                }
                self.asm.emit(bc::Op::BuildTuple(elements.len() as u32));
            }
            Expr::Dict(pairs) => {
                for (k, v) in pairs {
                    self.compile_expr(k)?;
                    self.compile_expr(v)?;
                }
                self.asm.emit(bc::Op::BuildDict(pairs.len() as u32));
            }
            Expr::Set(elements) => {
                for e in elements {
                    self.compile_expr(e)?;
                }
                self.asm.emit(bc::Op::BuildSet(elements.len() as u32));
            }
            Expr::ListComp { element, clauses } => {
                self.compile_comprehension(CompKind::List(element), clauses)?
            }
            Expr::DictComp {
                key,
                value,
                clauses,
            } => self.compile_comprehension(CompKind::Dict(key, value), clauses)?,
            Expr::SetComp { element, clauses } => {
                self.compile_comprehension(CompKind::Set(element), clauses)?
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
            Expr::BoolBinary { op, lhs, rhs } => self.compile_bool_binary(*op, lhs, rhs)?,
            Expr::Not { operand } => self.compile_not(operand)?,
            Expr::Conditional { test, body, orelse } => {
                self.compile_conditional(test, body, orelse)?
            }
            Expr::Compare { op, lhs, rhs } => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                match op {
                    ast::CmpOp::In => self.asm.emit(bc::Op::Contains { negate: false }),
                    ast::CmpOp::NotIn => self.asm.emit(bc::Op::Contains { negate: true }),
                    _ => self.asm.emit(bc::Op::Compare(cmp_sel(*op))),
                }
            }
            Expr::Call { func, args } => {
                if args.is_empty()
                    && matches!(&**func, Expr::Name(n) if n == "super")
                    && self.current_class.is_some()
                {
                    let class = self.current_class.as_ref().unwrap().clone();
                    let idx = self.name_index(&class);
                    self.asm.emit(bc::Op::LoadSuper(idx));
                } else {
                    self.compile_expr(func)?;
                    for arg in args {
                        self.compile_expr(arg)?;
                    }
                    self.asm.emit(bc::Op::Call(args.len() as u32));
                }
            }
        }
        Ok(())
    }

    /// Push a slice bound: the expression, or `None` when the bound is omitted.
    fn compile_slice_bound(&mut self, bound: &Option<Box<Expr>>) -> Result<(), CompileError> {
        match bound {
            Some(e) => self.compile_expr(e),
            None => {
                let none = self.const_index(bc::Const::None);
                self.asm.emit(bc::Op::LoadConst(none));
                Ok(())
            }
        }
    }

    /// Compile a comprehension: build an empty container in a temp, run the clause chain
    /// (nested loops, each with its `if` filters), append/insert the element at the innermost
    /// point, and leave the container on the stack.
    fn compile_comprehension(
        &mut self,
        kind: CompKind,
        clauses: &[CompClause],
    ) -> Result<(), CompileError> {
        let result = self.alloc_temp();
        let build = match kind {
            CompKind::List(_) => bc::Op::BuildList(0),
            CompKind::Set(_) => bc::Op::BuildSet(0),
            CompKind::Dict(..) => bc::Op::BuildDict(0),
        };
        self.asm.emit(build);
        self.asm.emit(bc::Op::StoreFast(result));
        self.compile_comp_clause(clauses, 0, result, &kind)?;
        self.asm.emit(bc::Op::LoadFast(result));
        Ok(())
    }

    /// Emit clause `i` and recurse into the rest (nesting the loops); at the innermost
    /// (`i == clauses.len()`) append/insert the element into `result`.
    fn compile_comp_clause(
        &mut self,
        clauses: &[CompClause],
        i: usize,
        result: u32,
        kind: &CompKind,
    ) -> Result<(), CompileError> {
        if i == clauses.len() {
            self.asm.emit(bc::Op::LoadFast(result));
            match *kind {
                CompKind::List(e) => {
                    self.compile_expr(e)?;
                    self.asm.emit(bc::Op::ListAppend);
                }
                CompKind::Set(e) => {
                    self.compile_expr(e)?;
                    self.asm.emit(bc::Op::SetAdd);
                }
                CompKind::Dict(k, v) => {
                    self.compile_expr(k)?;
                    self.compile_expr(v)?;
                    self.asm.emit(bc::Op::DictInsert);
                }
            }
            return Ok(());
        }
        let clause = &clauses[i];
        let iter = self.alloc_temp();
        self.compile_expr(&clause.iterable)?;
        self.asm.emit(bc::Op::GetIter);
        self.asm.emit(bc::Op::StoreFast(iter));
        let top = self.asm.new_label();
        let end = self.asm.new_label();
        self.asm.place(top);
        self.asm.emit(bc::Op::LoadFast(iter));
        self.asm.emit_for_iter(end);
        self.bind_comp_targets(&clause.targets);
        for cond in &clause.conditions {
            self.compile_expr(cond)?;
            self.asm.emit_branch(top);
        }
        self.compile_comp_clause(clauses, i + 1, result, kind)?;
        self.asm.emit_jump(top);
        self.asm.place(end);
        Ok(())
    }

    /// Bind a clause's target(s): a single name stores directly; a tuple target unpacks
    /// (`for k, v in d.items()`).
    fn bind_comp_targets(&mut self, targets: &[String]) {
        if targets.len() > 1 {
            self.asm.emit(bc::Op::UnpackSequence(targets.len() as u32));
        }
        for t in targets {
            let slot = self
                .local_slot(t)
                .expect("the comprehension target is a local (added by the pre-pass)");
            self.asm.emit(bc::Op::StoreFast(slot));
        }
    }

    /// Allocate a fresh synthetic local; its name begins with `.`, which no source
    /// identifier can, so it never collides with a user local.
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
        self.compile_expr(lhs)?;
        self.asm.emit(bc::Op::StoreFast(tmp));
        let end = self.asm.new_label();
        self.asm.emit(bc::Op::LoadFast(tmp));
        match op {
            BoolOp::And => {
                self.asm.emit_branch(end);
                self.compile_expr(rhs)?;
                self.asm.emit(bc::Op::StoreFast(tmp));
            }
            BoolOp::Or => {
                let eval_rhs = self.asm.new_label();
                self.asm.emit_branch(eval_rhs);
                self.asm.emit_jump(end);
                self.asm.place(eval_rhs);
                self.compile_expr(rhs)?;
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
        self.compile_expr(operand)?;
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
        self.compile_expr(test)?;
        let else_case = self.asm.new_label();
        let end = self.asm.new_label();
        self.asm.emit_branch(else_case);
        self.compile_expr(body)?;
        self.asm.emit(bc::Op::StoreFast(tmp));
        self.asm.emit_jump(end);
        self.asm.place(else_case);
        self.compile_expr(orelse)?;
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
        ast::CmpOp::In | ast::CmpOp::NotIn => {
            unreachable!("membership routes to Op::Contains")
        }
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
    /// Exception-table entries as `(body-start, body-end, handler, depth)` labels,
    /// resolved to op indices on `finish`.
    exc_entries: Vec<(Label, Label, Label, u32)>,
}

/// One pending emission: a ready op, or a jump whose target is a not-yet-resolved
/// label.
enum Emit {
    Op(bc::Op),
    Jump(Label),
    Branch(Label),
    ForIter(Label),
}

#[derive(Clone, Copy)]
struct Label(usize);

impl Assembler {
    fn new() -> Self {
        Assembler {
            emits: Vec::new(),
            labels: Vec::new(),
            cache_count: 0,
            exc_entries: Vec::new(),
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

    fn emit_for_iter(&mut self, target: Label) {
        self.emits.push(Emit::ForIter(target));
    }

    /// Record a protected `[start, end)` op range mapped to `handler` at stack `depth`.
    /// Resolved to op indices in `finish` (the table carries the cost, not the body).
    fn add_exc_entry(&mut self, start: Label, end: Label, handler: Label, depth: u32) {
        self.exc_entries.push((start, end, handler, depth));
    }

    /// Hand out the next inline-cache slot (ascending static order).
    fn next_cache_slot(&mut self) -> u32 {
        let slot = self.cache_count;
        self.cache_count += 1;
        slot
    }

    /// Resolve jump targets and produce the op stream, the inline-cache count, and the
    /// resolved exception table.
    fn finish(self) -> (Vec<bc::Op>, usize, Vec<bc::ExcEntry>) {
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
                Emit::ForIter(label) => bc::Op::ForIter(
                    self.labels[label.0].expect("every for-iter label is placed"),
                ),
            };
            ops.push(op);
        }
        let mut exc_table = Vec::with_capacity(self.exc_entries.len());
        for &(start, end, target, depth) in &self.exc_entries {
            exc_table.push(bc::ExcEntry {
                start: self.labels[start.0].expect("exc-table start placed"),
                end: self.labels[end.0].expect("exc-table end placed"),
                target: self.labels[target.0].expect("exc-table target placed"),
                depth,
            });
        }
        (ops, self.cache_count as usize, exc_table)
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
    fn a_nested_boolean_operator_compiles() {
        assert!(compile_src("def f(a: int, b: int) -> int:\n    return (a and b) + 1\n").is_ok());
    }

    #[test]
    fn a_conditional_expression_compiles_in_tail_position() {
        assert!(compile_src(
            "def f(a: int, b: int) -> int:\n    x = a if a > b else b\n    return x\n"
        )
        .is_ok());
    }

    #[test]
    fn a_nested_conditional_compiles() {
        assert!(compile_src("def f(a: int) -> int:\n    return 1 + (a if a else 0)\n").is_ok());
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
    fn break_or_continue_outside_a_loop_is_rejected() {
        assert!(compile_src("def f() -> int:\n    break\n    return 0\n").is_err());
        assert!(compile_src("def f() -> int:\n    continue\n    return 0\n").is_err());
    }

    #[test]
    fn break_and_continue_inside_a_loop_compile() {
        assert!(compile_src(
            "def f() -> int:\n    s = 0\n    for i in range(10):\n        if i == 5:\n            break\n        if i == 2:\n            continue\n        s += i\n    return s\n"
        )
        .is_ok());
    }

    #[test]
    fn class_def_emits_buildclass_methods_and_setattr() {
        let src = "class C:\n    def __init__(self, v):\n        self.v = v\n    def get(self):\n        return self.v\n\ndef main():\n    obj = C(5)\n    return obj.get()\n";
        let m = compile_src(src).unwrap();
        assert!(m.body.ops.iter().any(|op| matches!(op, Op::BuildClass)));
        assert_eq!(
            m.body.ops.iter().filter(|op| matches!(op, Op::MakeFunction(_))).count(),
            2
        );
        assert!(m.functions.iter().any(|f| f.name == "C.__init__"));
        assert!(m.functions.iter().any(|f| f.name == "C.get"));
        let init = m.functions.iter().find(|f| f.name == "C.__init__").unwrap();
        assert!(init.ops.iter().any(|op| matches!(op, Op::SetAttr { .. })));
    }

    #[test]
    fn super_call_in_method_emits_loadsuper() {
        let src = "class A:\n    def m(self):\n        return 1\n\nclass B(A):\n    def m(self):\n        return super().m() + 1\n\ndef main():\n    return B().m()\n";
        let m = compile_src(src).unwrap();
        let bm = m.functions.iter().find(|f| f.name == "B.m").unwrap();
        assert!(bm.ops.iter().any(|op| matches!(op, Op::LoadSuper(_))));
        let g = compile_src("def f():\n    return super()\n").unwrap();
        assert!(!func(&g, "f").ops.iter().any(|op| matches!(op, Op::LoadSuper(_))));
    }

    #[test]
    fn try_except_emits_the_exception_ops() {
        let src = "def f(x):\n    try:\n        x = g()\n    except E as e:\n        x = e\n    return x\n";
        let f = compile_src(src).unwrap();
        let co = func(&f, "f");
        assert_eq!(co.exc_table.len(), 1);
        let entry = co.exc_table[0];
        assert!(entry.start < entry.end, "the protected range is non-empty");
        assert!(entry.target >= entry.end, "the handler is after the body range");
        assert_eq!(entry.depth, 0, "a statement-level try restores to depth 0");
        let ops = &co.ops;
        assert!(ops.iter().any(|op| matches!(op, Op::MatchExc)));
        assert!(ops.iter().any(|op| matches!(op, Op::LoadExc)));
        assert!(ops.iter().any(|op| matches!(op, Op::PopExcept)));
        assert!(ops.iter().any(|op| matches!(op, Op::Reraise)));
    }

    #[test]
    fn raise_emits_raise_op() {
        let m = compile_src("def f(x):\n    raise x\n").unwrap();
        assert!(func(&m, "f").ops.iter().any(|op| matches!(op, Op::Raise(1))));
        let n = compile_src("def f():\n    raise\n").unwrap();
        assert!(func(&n, "f").ops.iter().any(|op| matches!(op, Op::Raise(0))));
    }

    #[test]
    fn raise_from_emits_raise_2() {
        let m = compile_src("def f(x, y):\n    raise x from y\n").unwrap();
        assert!(func(&m, "f").ops.iter().any(|op| matches!(op, Op::Raise(2))));
    }

    #[test]
    fn except_as_name_is_auto_deleted() {
        let m =
            compile_src("def f():\n    try:\n        pass\n    except E as e:\n        x = e\n")
                .unwrap();
        assert!(func(&m, "f").ops.iter().any(|op| matches!(op, Op::DeleteFast(_))));
    }

    #[test]
    fn try_finally_compiles_with_a_table_entry() {
        let m = compile_src("def f():\n    try:\n        pass\n    finally:\n        pass\n").unwrap();
        let co = func(&m, "f");
        assert_eq!(co.exc_table.len(), 1, "the finally adds one protected-range entry");
        assert!(co.ops.iter().any(|op| matches!(op, Op::Reraise)), "the finally copy reraises");
    }

    #[test]
    fn finally_duplicates_on_return_and_fallthrough() {
        let m = compile_src("def f():\n    try:\n        return 1\n    finally:\n        x = 2\n").unwrap();
        let co = func(&m, "f");
        assert_eq!(co.exc_table.len(), 1);
        let stores = co.ops.iter().filter(|op| matches!(op, Op::StoreFast(_))).count();
        assert!(stores >= 2, "finally (x = 2) duplicated at the return + the copy");
    }

    #[test]
    fn returning_finally_does_not_overflow() {
        let m =
            compile_src("def f():\n    try:\n        return 1\n    finally:\n        return 2\n")
                .unwrap();
        assert_eq!(func(&m, "f").exc_table.len(), 1);
    }

    #[test]
    fn membership_emits_contains() {
        let m = compile_src("def f(x, c):\n    return x in c\n").unwrap();
        assert!(func(&m, "f")
            .ops
            .iter()
            .any(|op| matches!(op, Op::Contains { negate: false })));
        let n = compile_src("def f(x, c):\n    return x not in c\n").unwrap();
        assert!(func(&n, "f")
            .ops
            .iter()
            .any(|op| matches!(op, Op::Contains { negate: true })));
    }

    #[test]
    fn setitem_emits_op_setitem() {
        let m = compile_src("def f(c, i, v):\n    c[i] = v\n    return v\n").unwrap();
        assert!(func(&m, "f").ops.iter().any(|op| matches!(op, Op::Setitem)));
    }

    #[test]
    fn for_over_an_iterable_emits_getiter_and_foriter() {
        let m = compile_src(
            "def f(items):\n    total = 0\n    for x in items:\n        total += x\n    return total\n",
        )
        .unwrap();
        let f = func(&m, "f");
        assert!(f.ops.iter().any(|op| matches!(op, Op::GetIter)));
        assert!(f.ops.iter().any(|op| matches!(op, Op::ForIter(_))));
    }

    #[test]
    fn tuple_and_dict_emit_their_build_ops() {
        let t = compile_src("def f(a):\n    return (a, a, a)\n").unwrap();
        assert!(func(&t, "f").ops.iter().any(|op| matches!(op, Op::BuildTuple(3))));
        let d = compile_src("def f(a):\n    return {a: a}\n").unwrap();
        assert!(func(&d, "f").ops.iter().any(|op| matches!(op, Op::BuildDict(1))));
    }

    #[test]
    fn tuple_assign_emits_unpack_sequence() {
        let m = compile_src("def f(p):\n    a, b = p\n    return a\n").unwrap();
        assert!(func(&m, "f").ops.iter().any(|op| matches!(op, Op::UnpackSequence(2))));
        let g = compile_src("def f(d):\n    s = 0\n    for k, v in d:\n        s = s + v\n    return s\n").unwrap();
        assert!(func(&g, "f").ops.iter().any(|op| matches!(op, Op::UnpackSequence(2))));
    }

    #[test]
    fn sets_emit_buildset_and_setadd() {
        let m = compile_src("def f():\n    return {1, 2, 3}\n").unwrap();
        assert!(func(&m, "f").ops.iter().any(|op| matches!(op, Op::BuildSet(3))));
        let c = compile_src("def f(r):\n    return {x for x in r}\n").unwrap();
        let ops = &func(&c, "f").ops;
        assert!(ops.iter().any(|op| matches!(op, Op::BuildSet(0))));
        assert!(ops.iter().any(|op| matches!(op, Op::SetAdd)));
    }

    #[test]
    fn comprehensions_emit_build_and_append() {
        let m = compile_src("def f(r):\n    return [x * 2 for x in r if x]\n").unwrap();
        let ops = &func(&m, "f").ops;
        assert!(ops.iter().any(|op| matches!(op, Op::BuildList(0))));
        assert!(ops.iter().any(|op| matches!(op, Op::ListAppend)));
        assert!(ops.iter().any(|op| matches!(op, Op::ForIter(_))));
        let d = compile_src("def f(r):\n    return {x: x for x in r}\n").unwrap();
        assert!(func(&d, "f").ops.iter().any(|op| matches!(op, Op::DictInsert)));
    }

    #[test]
    fn a_list_display_emits_buildlist() {
        let m = compile_src("def f(a, b):\n    return [a, b, a]\n").unwrap();
        let f = func(&m, "f");
        assert!(f.ops.iter().any(|op| matches!(op, Op::BuildList(3))));
    }

    #[test]
    fn a_slice_emits_buildslice_then_subscript() {
        let m = compile_src("def f(s):\n    return s[1:3]\n").unwrap();
        let f = func(&m, "f");
        let bs = f.ops.iter().position(|op| matches!(op, Op::BuildSlice));
        let sub = f.ops.iter().position(|op| matches!(op, Op::Subscript { .. }));
        assert!(bs.is_some(), "expected a BuildSlice");
        assert!(sub.is_some(), "expected a Subscript");
        assert!(bs < sub, "the slice is built before the subscript");
    }

    #[test]
    fn a_method_call_emits_loadattr_then_call() {
        let module = compile_src("def f(s, p):\n    return s.startswith(p)\n").unwrap();
        let f = func(&module, "f");
        let attr = f.ops.iter().position(|op| matches!(op, Op::LoadAttr { .. }));
        let call = f.ops.iter().position(|op| matches!(op, Op::Call(1)));
        assert!(attr.is_some(), "expected a LoadAttr for the method name");
        assert!(call.is_some(), "expected a Call(1)");
        assert!(attr < call, "the bound method must load before the call");
    }

    #[test]
    fn a_builtin_result_is_inferred_int() {
        let module =
            compile_src("def f(x: int) -> int:\n    y = abs(x)\n    return y\n").unwrap();
        let f = func(&module, "f");
        let y_slot = f.local_names.iter().position(|n| n == "y").unwrap();
        assert_eq!(f.local_types[y_slot], StaticType::Int);
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
