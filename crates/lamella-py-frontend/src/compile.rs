//! Lowering the AST to our bytecode.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
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
    let def_counts = direct_def_counts(&ast.body);
    let mut defs_seen: BTreeMap<String, usize> = BTreeMap::new();
    let func_rets = module_func_return_types(&ast.body);
    for stmt in &ast.body {
        match stmt {
            Stmt::FuncDef(func) => {
                let seen = defs_seen.entry(func.name.clone()).or_insert(0);
                *seen += 1;
                if *seen == def_counts[&func.name] {
                    let (co, lambdas) = compile_function(func, &func_rets)?;
                    functions.push(co);
                    functions.extend(lambdas);
                }
                top_level.push(stmt);
            }
            Stmt::ClassDef { name, body, .. } => {
                compile_class_method_bodies(name, body, &mut functions, &[], &func_rets)?;
                top_level.push(stmt);
            }
            Stmt::Decorated { inner, .. } => {
                match &**inner {
                    Stmt::FuncDef(func) => {
                        *defs_seen.entry(func.name.clone()).or_insert(0) += 1;
                    }
                    Stmt::ClassDef { name, body, .. } => {
                        compile_class_method_bodies(name, body, &mut functions, &[], &func_rets)?;
                    }
                    _ => {}
                }
                top_level.push(stmt);
            }
            other => top_level.push(other),
        }
    }
    let (body, body_lambdas) =
        compile_code_object(Scope::Module, "<module>", &[], &None, &top_level, None, Outer { enclosing: &[], func_rets: &func_rets })?;
    functions.extend(body_lambdas);
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

fn compile_function(
    func: &FuncDef,
    func_rets: &BTreeMap<String, bc::StaticType>,
) -> Result<(bc::CodeObject, Vec<bc::CodeObject>), CompileError> {
    let body: Vec<&Stmt> = func.body.iter().collect();
    compile_code_object(Scope::Function, &func.name, &func.params, &func.ret, &body, None, Outer { enclosing: &[], func_rets })
}

/// The simple method name of a class-body member (a `def` or a decorated `def`), else `None`.
fn class_member_method_name(member: &Stmt) -> Option<&str> {
    match member {
        Stmt::FuncDef(m) => Some(&m.name),
        Stmt::Decorated { inner, .. } => match &**inner {
            Stmt::FuncDef(m) => Some(&m.name),
            _ => None,
        },
        _ => None,
    }
}

/// The unique qualified name for each class-body method, disambiguating siblings that share a name
/// (a `@property` getter and its `@x.setter` are both `def x`, and must be DISTINCT code objects):
/// the first occurrence is `Class.name`, a later one `Class.name$1`, `$2`, ... (`$` cannot appear in
/// a source identifier, so it never clashes with a real method). Returns one entry per body member
/// (`None` for a non-method), index-aligned so the body compiler and the emitter agree on the name.
fn method_qualified_names(class_name: &str, body: &[Stmt]) -> Vec<Option<String>> {
    body.iter()
        .enumerate()
        .map(|(i, member)| {
            let mn = class_member_method_name(member)?;
            let prior = body[..i]
                .iter()
                .filter(|m| class_member_method_name(m) == Some(mn))
                .count();
            Some(if prior == 0 {
                format!("{class_name}.{mn}")
            } else {
                format!("{class_name}.{mn}${prior}")
            })
        })
        .collect()
}

/// Compile a class method as a Module function named `qualified` (`"ClassName.method"`, or a
/// `$`-suffixed variant for a same-named sibling); the class body emits `MakeFunction` referencing it
/// by that same qualified name.
fn compile_method(
    qualified: &str,
    class_name: &str,
    method: &FuncDef,
    enclosing: &[BTreeSet<String>],
    func_rets: &BTreeMap<String, bc::StaticType>,
) -> Result<(bc::CodeObject, Vec<bc::CodeObject>), CompileError> {
    let body: Vec<&Stmt> = method.body.iter().collect();
    compile_code_object(
        Scope::Function,
        qualified,
        &method.params,
        &method.ret,
        &body,
        Some(class_name),
        Outer { enclosing, func_rets },
    )
}

/// Compile every method in a class body -- bare `def` or decorated -- as a Module function named
/// "Class.method", appending each (and any lambdas it hoists) to `functions`. The class body's
/// namespace dict references them by that qualified name; a decorated method also wraps the
/// reference with its decorators at the def site.
fn compile_class_method_bodies(
    class_name: &str,
    body: &[Stmt],
    functions: &mut Vec<bc::CodeObject>,
    enclosing: &[BTreeSet<String>],
    func_rets: &BTreeMap<String, bc::StaticType>,
) -> Result<(), CompileError> {
    let names = method_qualified_names(class_name, body);
    for (i, member) in body.iter().enumerate() {
        let method = match member {
            Stmt::FuncDef(m) => Some(m),
            Stmt::Decorated { inner, .. } => match &**inner {
                Stmt::FuncDef(m) => Some(m),
                _ => None,
            },
            _ => None,
        };
        if let Some(method) = method {
            let qualified = names[i].as_ref().expect("a method has a qualified name");
            let (co, lambdas) = compile_method(qualified, class_name, method, enclosing, func_rets)?;
            functions.push(co);
            functions.extend(lambdas);
        }
    }
    Ok(())
}

/// What a code object compiles against from OUTSIDE itself: the scope chain of the functions it sits
/// inside, and the module functions whose return type a call in it may resolve. They travel together
/// because they answer the same question -- what a name that is not local to this body means -- and the
/// first decides whether the second may be trusted (a scope with an enclosing function can capture a
/// name as a freevar, shadowing a module function invisibly).
#[derive(Clone, Copy)]
struct Outer<'a> {
    enclosing: &'a [BTreeSet<String>],
    func_rets: &'a BTreeMap<String, bc::StaticType>,
}

/// Each module-level function's DECLARED return type, so a call to one types the local it is bound to
/// (`total = compute(xs)`). The lowering already resolves a call against the callee's signature; this is
/// what lets the INFERENCE agree with it, instead of leaving the local Dynamic and taking every later
/// use of it off the typed lane.
///
/// A def the module REBINDS contributes the last one's annotation, matching which body the bare name
/// ends up calling. An unannotated function maps to `Dynamic`, which is what its calls already inferred.
fn module_func_return_types(body: &[Stmt]) -> BTreeMap<String, bc::StaticType> {
    let mut rets = BTreeMap::new();
    for stmt in body {
        let def = match stmt {
            Stmt::FuncDef(func) => Some(func),
            Stmt::Decorated { inner, .. } => match &**inner {
                Stmt::FuncDef(_) => None,
                _ => None,
            },
            _ => None,
        };
        if let Some(func) = def {
            rets.insert(func.name.clone(), resolve_type(&func.ret));
        }
    }
    rets
}

/// How many times each name is `def`ed DIRECTLY in this body (a def nested in a block is not counted:
/// it never owns a table entry). A later `def` REBINDS the name, so only the LAST def of a name owns
/// it at module scope -- the function-table entry callable by that name, and what a typed caller
/// resolves. An earlier same-named def still binds the name for as long as it stands, so it is
/// hoisted under a unique synthetic name at its def site, like a block-nested def.
fn direct_def_counts<'a>(body: impl IntoIterator<Item = &'a Stmt>) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for stmt in body {
        let def = match stmt {
            Stmt::FuncDef(func) => Some(func),
            Stmt::Decorated { inner, .. } => match &**inner {
                Stmt::FuncDef(func) => Some(func),
                _ => None,
            },
            _ => None,
        };
        if let Some(func) = def {
            *counts.entry(func.name.clone()).or_insert(0) += 1;
        }
    }
    counts
}

/// The names a class body binds: member assignment targets + method names (bare or decorated). A
/// read of one of these inside the body is a class-local, resolved namespace-first (`LoadName`) so it
/// reads the class attribute rather than a shadowed enclosing/module name.
fn class_body_bound_names(body: &[Stmt]) -> BTreeSet<String> {
    let mut bound = BTreeSet::new();
    for member in body {
        if let Stmt::Assign(a) = member {
            bound.insert(a.target.clone());
        } else if let Some(method) = class_member_method_name(member) {
            bound.insert(String::from(method));
        }
    }
    bound
}

/// Resolve an annotation expression to a static type: a bare `int` is the typed
/// integer path; everything else (including no annotation) is dynamic.
fn resolve_type(annotation: &Option<Expr>) -> bc::StaticType {
    match annotation {
        Some(Expr::Name(name)) if name == "int" => bc::StaticType::Int,
        Some(Expr::Name(name)) if name == "float" => bc::StaticType::Float,
        Some(Expr::Subscript { value, index }) if matches!(&**value, Expr::Name(n) if n == "list") => {
            match &**index {
                Expr::Name(elem) if elem == "int" => bc::StaticType::ListInt,
                Expr::Name(elem) if elem == "float" => bc::StaticType::ListFloat,
                _ => bc::StaticType::Dynamic,
            }
        }
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
    outer: Outer<'_>,
) -> Result<(bc::CodeObject, Vec<bc::CodeObject>), CompileError> {
    let Outer { enclosing, func_rets } = outer;
    let mut local_names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
    let mut local_types: Vec<bc::StaticType> =
        params.iter().map(|p| resolve_type(&p.annotation)).collect();
    collect_locals(body, &mut local_names, &mut local_types);
    for stmt in body {
        collect_comp_targets_stmt(stmt, &mut local_names, &mut local_types);
    }
    let mut nonlocals = BTreeSet::new();
    collect_nonlocals(body, &mut nonlocals);
    if !nonlocals.is_empty() {
        for n in &nonlocals {
            if params.iter().any(|p| &p.name == n) {
                return Err(error(&format!("name '{n}' is parameter and nonlocal")));
            }
            if scope == Scope::Module || !enclosing.iter().any(|s| s.contains(n)) {
                return Err(error(&format!("no binding for nonlocal '{n}' found")));
            }
        }
        let mut kept_names = Vec::with_capacity(local_names.len());
        let mut kept_types = Vec::with_capacity(local_types.len());
        for (name, ty) in local_names.iter().zip(&local_types) {
            if !nonlocals.contains(name) {
                kept_names.push(name.clone());
                kept_types.push(*ty);
            }
        }
        local_names = kept_names;
        local_types = kept_types;
    }
    let mut globals = BTreeSet::new();
    collect_globals(body, &mut globals);
    if !globals.is_empty() {
        for n in &globals {
            if params.iter().any(|p| &p.name == n) {
                return Err(error(&format!("name '{n}' is parameter and global")));
            }
        }
        let mut kept_names = Vec::with_capacity(local_names.len());
        let mut kept_types = Vec::with_capacity(local_types.len());
        for (name, ty) in local_names.iter().zip(&local_types) {
            if !globals.contains(name) {
                kept_names.push(name.clone());
                kept_types.push(*ty);
            }
        }
        local_names = kept_names;
        local_types = kept_types;
    }
    let no_rets = BTreeMap::new();
    let visible_rets = if enclosing.is_empty() { func_rets } else { &no_rets };
    infer_local_types(params, body, &local_names, &mut local_types, visible_rets);

    let mut bound = bound_names(params, body);
    for n in &globals {
        bound.remove(n);
    }
    let (cellvars, freevars) = if scope == Scope::Module {
        (Vec::new(), Vec::new())
    } else {
        let mut u = Uses::default();
        for stmt in body {
            walk_stmt_uses(stmt, &mut u);
        }
        let cellvars: Vec<String> = bound.intersection(&u.child_free).cloned().collect();
        let mut free_uses = u.direct;
        free_uses.extend(u.child_free);
        free_uses.extend(nonlocals.iter().cloned());
        for n in &bound {
            free_uses.remove(n);
        }
        for n in &globals {
            free_uses.remove(n);
        }
        let freevars: Vec<String> = free_uses
            .into_iter()
            .filter(|n| enclosing.iter().any(|s| s.contains(n)))
            .collect();
        (cellvars, freevars)
    };
    let mut child_scopes: Vec<BTreeSet<String>> = enclosing.to_vec();
    if scope == Scope::Function {
        child_scopes.push(bound);
    }

    let mut compiler = Compiler {
        scope,
        asm: Assembler::new(),
        consts: Vec::new(),
        names: Vec::new(),
        local_names,
        local_types,
        cellvars: cellvars.clone(),
        freevars: freevars.clone(),
        globals,
        child_scopes,
        loops: Vec::new(),
        finallys: Vec::new(),
        handler_depth: 0,
        in_class_body: false,
        class_body_bound: BTreeSet::new(),
        current_class: current_class.map(String::from),
        name: String::from(name),
        hoisted: Vec::new(),
        lambda_counter: 0,
        has_yield: false,
        direct_body_stmt: false,
        decorating_a_def: false,
        block_def_counter: 0,
        nested_def_counts: BTreeMap::new(),
        module_def_totals: direct_def_counts(body.iter().copied()),
        module_defs_seen: BTreeMap::new(),
        func_rets: visible_rets.clone(),
    };
    for stmt in body {
        compiler.direct_body_stmt = true;
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
    let code_object = bc::CodeObject {
        name: String::from(name),
        params: co_params,
        posonly_count: params.iter().filter(|p| p.positional_only).count() as u32,
        kwonly_count: params.iter().filter(|p| p.keyword_only).count() as u32,
        is_generator: compiler.has_yield,
        has_varargs: params.iter().any(|p| p.is_vararg),
        has_varkwargs: params.iter().any(|p| p.is_varkwarg),
        ret_ty: resolve_type(ret),
        n_locals: compiler.local_names.len(),
        local_names: compiler.local_names,
        cellvars,
        freevars,
        local_types: compiler.local_types,
        consts: compiler.consts,
        names: compiler.names,
        ops,
        cache_count,
        exc_table,
    };
    Ok((code_object, compiler.hoisted))
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
        Stmt::MultiAssign { targets, .. } => {
            for target in targets {
                let mut bound = Vec::new();
                target.collect_names(&mut bound);
                for name in bound {
                    add_dynamic_local(name, names, types);
                }
            }
        }
        Stmt::TupleAssign { targets, .. } => {
            for target in targets {
                let mut bound = Vec::new();
                target.collect_names(&mut bound);
                for name in bound {
                    add_dynamic_local(name, names, types);
                }
            }
        }
        Stmt::Import { modules } => {
            for (_, bound) in modules {
                add_dynamic_local(bound, names, types);
            }
        }
        Stmt::ImportFrom { names: imports, .. } => {
            for (_, bound) in imports {
                add_dynamic_local(bound, names, types);
            }
        }
        Stmt::ImportStar { .. } => {}
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
        Stmt::With {
            optional_name,
            body,
            ..
        } => {
            if let Some(name) = optional_name {
                add_dynamic_local(name, names, types);
            }
            for s in body {
                collect_locals_stmt(s, names, types);
            }
        }
        Stmt::ClassDef { name, .. } => {
            if !names.iter().any(|n| n == name) {
                names.push(name.clone());
                types.push(bc::StaticType::Dynamic);
            }
        }
        Stmt::FuncDef(func) => {
            add_dynamic_local(&func.name, names, types);
        }
        Stmt::Decorated { decorators, inner } => {
            match &**inner {
                Stmt::FuncDef(f) => add_dynamic_local(&f.name, names, types),
                Stmt::ClassDef { name, .. } => add_dynamic_local(name, names, types),
                _ => {}
            }
            collect_locals_stmt(inner, names, types);
            for d in decorators {
                collect_comp_targets_expr(d, names, types);
            }
        }
        Stmt::Return(_)
        | Stmt::Expr(_)
        | Stmt::Delete(_)
        | Stmt::Nonlocal(_)
        | Stmt::Global(_)
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
            ..
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
        Stmt::With { context, body, .. } => {
            collect_comp_targets_expr(context, names, types);
            for s in body {
                collect_comp_targets_stmt(s, names, types);
            }
        }
        Stmt::Decorated { decorators, .. } => {
            for d in decorators {
                collect_comp_targets_expr(d, names, types);
            }
        }
        Stmt::Return(None)
        | Stmt::FuncDef(_)
        | Stmt::ClassDef { .. }
        | Stmt::Import { .. }
        | Stmt::ImportFrom { .. }
        | Stmt::ImportStar { .. }
        | Stmt::Nonlocal(_)
        | Stmt::Global(_)
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Pass
        | Stmt::Delete(_) => {}
    }
}

/// The inner value expression of a call argument -- the same field for every kind (`value`,
/// `*value`, `name=value`, `**value`).
fn call_arg_expr(arg: &ast::CallArg) -> &Expr {
    match arg {
        ast::CallArg::Positional(e)
        | ast::CallArg::Star(e)
        | ast::CallArg::Keyword(_, e)
        | ast::CallArg::DoubleStar(e) => e,
    }
}

/// Build the body of the hidden generator function a generator expression compiles to (CPython's
/// `<genexpr>`): the `for`/`if` clause chain wrapping `yield element`, with the OUTERMOST clause
/// iterating the `.0` parameter -- the pre-evaluated, pre-iter'd outermost iterable. A multi-name
/// clause target unpacks through a temp. Shared by the emitter and the free-variable analysis so
/// both see the same synthetic scope.
fn build_genexpr_body(element: &Expr, clauses: &[CompClause]) -> Vec<Stmt> {
    wrap_comp_clauses(
        vec![Stmt::Expr(Expr::Yield(Some(Box::new(element.clone()))))],
        clauses,
    )
}

/// Wrap `innermost` in a comprehension's nested for/if clause chain (shared by every comprehension
/// kind's hidden function). The OUTERMOST clause iterates the `.0` parameter -- the eagerly-iter'd
/// first iterable the call site passes in; inner clauses iterate their own expressions.
fn wrap_comp_clauses(mut body: Vec<Stmt>, clauses: &[CompClause]) -> Vec<Stmt> {
    for (i, clause) in clauses.iter().enumerate().rev() {
        for cond in clause.conditions.iter().rev() {
            body = vec![Stmt::If {
                test: cond.clone(),
                body,
                orelse: Vec::new(),
            }];
        }
        let iterable = if i == 0 {
            Expr::Name(String::from(".0"))
        } else {
            clause.iterable.clone()
        };
        body = if clause.targets.len() == 1 {
            vec![Stmt::ForIter {
                target: clause.targets[0].clone(),
                iterable,
                body,
                orelse: Vec::new(),
            }]
        } else {
            let loopvar = format!(".g{i}");
            let mut inner = vec![Stmt::TupleAssign {
                targets: clause
                    .targets
                    .iter()
                    .map(|t| ast::AssignTarget::Name(t.clone()))
                    .collect(),
                star: None,
                value: Expr::Name(loopvar.clone()),
            }];
            inner.extend(body);
            vec![Stmt::ForIter {
                target: loopvar,
                iterable,
                body: inner,
                orelse: Vec::new(),
            }]
        };
    }
    body
}

/// The single `.0` parameter of a comprehension's hidden function (the outermost iterable).
fn genexpr_param() -> ast::ParamDef {
    ast::ParamDef {
        name: String::from(".0"),
        annotation: None,
        default: None,
        keyword_only: false,
        positional_only: false,
        is_vararg: false,
        is_varkwarg: false,
    }
}

/// The body of a list/set/dict comprehension's hidden function: build an empty accumulator, add each
/// element as the clause chain runs, and return it. The accumulator name begins with `.`, which no
/// source identifier can, so it never collides with a loop target or a captured name. An empty
/// `[]` / set / `{}` display and the `.append`/`.add`/`[k]=v` on that fresh container are all
/// shadow-proof -- they never touch the `list`/`set`/`dict` builtins (which a user may have rebound).
fn build_container_comp_body(kind: CompKind, clauses: &[CompClause]) -> Vec<Stmt> {
    let acc = String::from(".acc");
    let (init, add): (Expr, Stmt) = match kind {
        CompKind::List(e) => (
            Expr::List(Vec::new()),
            method_call_stmt(&acc, "append", vec![e.clone()]),
        ),
        CompKind::Set(e) => (
            Expr::Set(Vec::new()),
            method_call_stmt(&acc, "add", vec![e.clone()]),
        ),
        CompKind::Dict(k, v) => (
            Expr::Dict(Vec::new()),
            Stmt::SetItem {
                container: Expr::Name(acc.clone()),
                index: k.clone(),
                value: v.clone(),
                op: None,
            },
        ),
    };
    let mut body = vec![Stmt::Assign(Assign {
        target: acc.clone(),
        annotation: None,
        value: Some(init),
    })];
    body.extend(wrap_comp_clauses(vec![add], clauses));
    body.push(Stmt::Return(Some(Expr::Name(acc))));
    body
}

/// A statement `obj.method(args...)` (result discarded) -- the accumulator append/add.
fn method_call_stmt(obj: &str, method: &str, args: Vec<Expr>) -> Stmt {
    Stmt::Expr(Expr::Call {
        func: Box::new(Expr::Attribute {
            value: Box::new(Expr::Name(String::from(obj))),
            attr: String::from(method),
        }),
        args,
        keywords: Vec::new(),
    })
}

/// Whether a comprehension may compile to its own function scope (so its loop targets do not leak
/// into the enclosing scope). It may UNLESS a walrus (`:=`) appears in a part that would move into
/// that scope -- the element/key/value, a condition, or a non-outermost iterable -- because a walrus
/// must bind in the containing scope (PEP 572), which only the inline form does. (A walrus in the
/// outermost iterable is evaluated in the enclosing scope either way, so it does not force inline.)
fn comprehension_hoists(main_exprs: &[&Expr], clauses: &[CompClause]) -> bool {
    let mut u = Uses::default();
    for e in main_exprs {
        walk_expr_uses(e, &mut u);
    }
    for (i, c) in clauses.iter().enumerate() {
        for cond in &c.conditions {
            walk_expr_uses(cond, &mut u);
        }
        if i > 0 {
            walk_expr_uses(&c.iterable, &mut u);
        }
    }
    !u.has_walrus
}

/// The free variables of a hoisted comprehension's hidden function -- the names it captures from the
/// enclosing scope (so they become cells). A genexpr-shaped body over the same element(s) and clauses
/// has the identical free-variable profile (the accumulator name is bound, and the container ops name
/// no variables), so this reuses that skeleton; a dict's key and value are combined so both count.
fn comp_free_uses(main_exprs: &[&Expr], clauses: &[CompClause]) -> BTreeSet<String> {
    let element = if main_exprs.len() == 1 {
        main_exprs[0].clone()
    } else {
        Expr::Tuple(main_exprs.iter().map(|e| (*e).clone()).collect())
    };
    let body = build_genexpr_body(&element, clauses);
    let refs: Vec<&Stmt> = body.iter().collect();
    func_free_uses(&[genexpr_param()], &refs)
}

/// Walk an expression, collecting comprehension loop variables (recursing into nested
/// comprehensions) as dynamic locals.
fn collect_comp_targets_expr(
    expr: &Expr,
    names: &mut Vec<String>,
    types: &mut Vec<bc::StaticType>,
) {
    match expr {
        Expr::ListComp { element, clauses }
        | Expr::SetComp { element, clauses } => {
            if comprehension_hoists(&[element], clauses) {
                if let Some(first) = clauses.first() {
                    collect_comp_targets_expr(&first.iterable, names, types);
                }
            } else {
                collect_comp_clauses(clauses, names, types);
                collect_comp_targets_expr(element, names, types);
            }
        }
        Expr::GeneratorExp { clauses, .. } => {
            if let Some(first) = clauses.first() {
                collect_comp_targets_expr(&first.iterable, names, types);
            }
        }
        Expr::DictComp { key, value, clauses } => {
            if comprehension_hoists(&[key, value], clauses) {
                if let Some(first) = clauses.first() {
                    collect_comp_targets_expr(&first.iterable, names, types);
                }
            } else {
                collect_comp_clauses(clauses, names, types);
                collect_comp_targets_expr(key, names, types);
                collect_comp_targets_expr(value, names, types);
            }
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::InplaceBinary { lhs, rhs, .. }
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
        Expr::Call { func, args, keywords } => {
            collect_comp_targets_expr(func, names, types);
            for a in args {
                collect_comp_targets_expr(a, names, types);
            }
            for k in keywords {
                collect_comp_targets_expr(&k.value, names, types);
            }
        }
        Expr::CallEx { func, args } => {
            collect_comp_targets_expr(func, names, types);
            for a in args {
                collect_comp_targets_expr(call_arg_expr(a), names, types);
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
        Expr::Walrus { target, value } => {
            add_dynamic_local(target, names, types);
            collect_comp_targets_expr(value, names, types);
        }
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Imaginary(_)
        | Expr::BigInt(_)
        | Expr::Bytes(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::None
        | Expr::Name(_) => {}
        Expr::Lambda { .. } => {}
        Expr::Yield(value) => {
            if let Some(v) = value {
                collect_comp_targets_expr(v, names, types);
            }
        }
        Expr::YieldFrom(value) => collect_comp_targets_expr(value, names, types),
    }
}


/// The name references a scope contributes to the closure analysis.
#[derive(Default)]
struct Uses {
    /// Names read directly in this scope's own expressions.
    direct: BTreeSet<String>,
    /// Names that functions nested in this scope need from this scope or an outer one -- a nested
    /// function's free variables, bubbled up so an enclosing scope can satisfy them with a cell.
    child_free: BTreeSet<String>,
    /// Set when a walrus (`:=`) was seen while walking. Used only to decide whether a comprehension
    /// can move to its own function scope (a walrus must bind in the containing scope, so it cannot).
    has_walrus: bool,
}

/// The user-visible names a function scope binds: its parameters plus every name assigned in its
/// body plus comprehension/walrus targets -- exactly the set the pre-pass turns into local slots.
fn bound_names(params: &[ast::ParamDef], body: &[&Stmt]) -> BTreeSet<String> {
    let mut names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
    let mut types: Vec<bc::StaticType> = vec![bc::StaticType::Dynamic; names.len()];
    collect_locals(body, &mut names, &mut types);
    for stmt in body {
        collect_comp_targets_stmt(stmt, &mut names, &mut types);
    }
    let mut set: BTreeSet<String> = names.into_iter().collect();
    let mut nonlocals = BTreeSet::new();
    collect_nonlocals(body, &mut nonlocals);
    for n in &nonlocals {
        set.remove(n);
    }
    set
}

/// Collect the names declared `nonlocal` anywhere in a function body -- descending into control-flow
/// blocks but NOT into nested functions (a `nonlocal` declaration binds the function it appears in).
fn collect_nonlocals(body: &[&Stmt], out: &mut BTreeSet<String>) {
    for stmt in body {
        collect_nonlocals_stmt(stmt, out);
    }
}

fn collect_nonlocals_stmt(stmt: &Stmt, out: &mut BTreeSet<String>) {
    match stmt {
        Stmt::Nonlocal(names) => {
            for n in names {
                out.insert(n.clone());
            }
        }
        Stmt::If { body, orelse, .. } | Stmt::While { body, orelse, .. } => {
            for s in body.iter().chain(orelse.iter()) {
                collect_nonlocals_stmt(s, out);
            }
        }
        Stmt::For { body, orelse, .. } | Stmt::ForIter { body, orelse, .. } => {
            for s in body.iter().chain(orelse.iter()) {
                collect_nonlocals_stmt(s, out);
            }
        }
        Stmt::Try { body, handlers, orelse, finalbody } => {
            for s in body.iter().chain(orelse.iter()).chain(finalbody.iter()) {
                collect_nonlocals_stmt(s, out);
            }
            for h in handlers {
                for s in &h.body {
                    collect_nonlocals_stmt(s, out);
                }
            }
        }
        Stmt::With { body, .. } => {
            for s in body {
                collect_nonlocals_stmt(s, out);
            }
        }
        _ => {}
    }
}

/// Collect the names declared `global` anywhere in a function body -- descending into control-flow
/// blocks but NOT into nested functions (a `global` declaration binds only the function it is in).
fn collect_globals(body: &[&Stmt], out: &mut BTreeSet<String>) {
    for stmt in body {
        collect_globals_stmt(stmt, out);
    }
}

fn collect_globals_stmt(stmt: &Stmt, out: &mut BTreeSet<String>) {
    match stmt {
        Stmt::Global(names) => {
            for n in names {
                out.insert(n.clone());
            }
        }
        Stmt::If { body, orelse, .. } | Stmt::While { body, orelse, .. } => {
            for s in body.iter().chain(orelse.iter()) {
                collect_globals_stmt(s, out);
            }
        }
        Stmt::For { body, orelse, .. } | Stmt::ForIter { body, orelse, .. } => {
            for s in body.iter().chain(orelse.iter()) {
                collect_globals_stmt(s, out);
            }
        }
        Stmt::Try { body, handlers, orelse, finalbody } => {
            for s in body.iter().chain(orelse.iter()).chain(finalbody.iter()) {
                collect_globals_stmt(s, out);
            }
            for h in handlers {
                for s in &h.body {
                    collect_globals_stmt(s, out);
                }
            }
        }
        Stmt::With { body, .. } => {
            for s in body {
                collect_globals_stmt(s, out);
            }
        }
        _ => {}
    }
}

/// The free variables of a function scope: names its subtree references that it does not bind
/// locally. Each such name resolves, in an enclosing scope, either to a cell (an enclosing
/// function local) or to a global.
fn func_free_uses(params: &[ast::ParamDef], body: &[&Stmt]) -> BTreeSet<String> {
    let mut u = Uses::default();
    for stmt in body {
        walk_stmt_uses(stmt, &mut u);
    }
    let bound = bound_names(params, body);
    let mut all = u.direct;
    all.extend(u.child_free);
    collect_nonlocals(body, &mut all);
    all.difference(&bound).cloned().collect()
}

/// Accumulate `stmt`'s name references into `u`, descending into nested `def` bodies as child
/// scopes (their free variables bubble into `child_free`; their parameter defaults are uses of
/// THIS scope).
fn walk_stmt_uses(stmt: &Stmt, u: &mut Uses) {
    match stmt {
        Stmt::FuncDef(f) => {
            let body: Vec<&Stmt> = f.body.iter().collect();
            u.child_free.extend(func_free_uses(&f.params, &body));
            for p in &f.params {
                if let Some(d) = &p.default {
                    walk_expr_uses(d, u);
                }
            }
        }
        Stmt::Decorated { decorators, inner } => {
            for d in decorators {
                walk_expr_uses(d, u);
            }
            walk_stmt_uses(inner, u);
        }
        Stmt::Return(value) => {
            if let Some(e) = value {
                walk_expr_uses(e, u);
            }
        }
        Stmt::Assign(a) => {
            if let Some(v) = &a.value {
                walk_expr_uses(v, u);
            }
        }
        Stmt::MultiAssign { targets, value } => {
            walk_expr_uses(value, u);
            for t in targets {
                walk_target_uses(t, u);
            }
        }
        Stmt::TupleAssign { targets, value, .. } => {
            walk_expr_uses(value, u);
            for t in targets {
                walk_target_uses(t, u);
            }
        }
        Stmt::SetItem { container, index, value, .. } => {
            walk_expr_uses(container, u);
            walk_expr_uses(index, u);
            walk_expr_uses(value, u);
        }
        Stmt::SetAttr { obj, value, .. } => {
            walk_expr_uses(obj, u);
            walk_expr_uses(value, u);
        }
        Stmt::Expr(e) => walk_expr_uses(e, u),
        Stmt::Delete(targets) => {
            for t in targets {
                walk_expr_uses(t, u);
            }
        }
        Stmt::If { test, body, orelse } => {
            walk_expr_uses(test, u);
            walk_body_uses(body, u);
            walk_body_uses(orelse, u);
        }
        Stmt::While { test, body, orelse } => {
            walk_expr_uses(test, u);
            walk_body_uses(body, u);
            walk_body_uses(orelse, u);
        }
        Stmt::For { start, stop, body, orelse, .. } => {
            walk_expr_uses(start, u);
            walk_expr_uses(stop, u);
            walk_body_uses(body, u);
            walk_body_uses(orelse, u);
        }
        Stmt::ForIter { iterable, body, orelse, .. } => {
            walk_expr_uses(iterable, u);
            walk_body_uses(body, u);
            walk_body_uses(orelse, u);
        }
        Stmt::Raise { exc, cause } => {
            if let Some(e) = exc {
                walk_expr_uses(e, u);
            }
            if let Some(c) = cause {
                walk_expr_uses(c, u);
            }
        }
        Stmt::Try { body, handlers, orelse, finalbody } => {
            walk_body_uses(body, u);
            for h in handlers {
                if let Some(t) = &h.typ {
                    walk_expr_uses(t, u);
                }
                walk_body_uses(&h.body, u);
            }
            walk_body_uses(orelse, u);
            walk_body_uses(finalbody, u);
        }
        Stmt::With { context, body, .. } => {
            walk_expr_uses(context, u);
            walk_body_uses(body, u);
        }
        Stmt::ClassDef { bases, body, .. } => {
            for b in bases {
                walk_expr_uses(b, u);
            }
            walk_body_uses(body, u);
        }
        Stmt::Import { .. }
        | Stmt::ImportFrom { .. }
        | Stmt::ImportStar { .. }
        | Stmt::Nonlocal(_)
        | Stmt::Global(_)
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Pass => {}
    }
}

/// Accumulate the name references of every statement in `body`.
fn walk_body_uses(body: &[Stmt], u: &mut Uses) {
    for stmt in body {
        walk_stmt_uses(stmt, u);
    }
}

/// Accumulate the name references in a store target -- a bare name binds (no use), while a
/// subscript/attribute target uses the container/object (and index) it stores through.
fn walk_target_uses(target: &ast::AssignTarget, u: &mut Uses) {
    match target {
        ast::AssignTarget::Name(_) => {}
        ast::AssignTarget::Subscript { container, index } => {
            walk_expr_uses(container, u);
            walk_expr_uses(index, u);
        }
        ast::AssignTarget::Attribute { obj, .. } => walk_expr_uses(obj, u),
        ast::AssignTarget::Tuple(targets) => {
            for t in targets {
                walk_target_uses(t, u);
            }
        }
    }
}

/// Accumulate `expr`'s name references into `u`. A nested `lambda` is a child scope (its body's
/// free variables bubble into `child_free`); every other form recurses. Comprehension targets are
/// this scope's locals (the compiler inlines comprehensions), so a clause's iterable/conditions are
/// this-scope uses.
fn walk_expr_uses(expr: &Expr, u: &mut Uses) {
    match expr {
        Expr::Name(n) => {
            u.direct.insert(n.clone());
        }
        Expr::Int(_) | Expr::Float(_) | Expr::Imaginary(_) | Expr::BigInt(_) | Expr::Bytes(_)
        | Expr::Str(_) | Expr::Bool(_) | Expr::None => {}
        Expr::Lambda { params, body } => {
            let ret = Stmt::Return(Some((**body).clone()));
            let refs: Vec<&Stmt> = vec![&ret];
            u.child_free.extend(func_free_uses(params, &refs));
            for p in params {
                if let Some(d) = &p.default {
                    walk_expr_uses(d, u);
                }
            }
        }
        Expr::Attribute { value, .. } => walk_expr_uses(value, u),
        Expr::Binary { lhs, rhs, .. }
        | Expr::InplaceBinary { lhs, rhs, .. }
        | Expr::BoolBinary { lhs, rhs, .. }
        | Expr::Compare { lhs, rhs, .. } => {
            walk_expr_uses(lhs, u);
            walk_expr_uses(rhs, u);
        }
        Expr::Unary { operand, .. } | Expr::Not { operand } => walk_expr_uses(operand, u),
        Expr::Conditional { test, body, orelse } => {
            walk_expr_uses(test, u);
            walk_expr_uses(body, u);
            walk_expr_uses(orelse, u);
        }
        Expr::Call { func, args, keywords } => {
            walk_expr_uses(func, u);
            for a in args {
                walk_expr_uses(a, u);
            }
            for k in keywords {
                walk_expr_uses(&k.value, u);
            }
        }
        Expr::CallEx { func, args } => {
            walk_expr_uses(func, u);
            for a in args {
                walk_expr_uses(call_arg_expr(a), u);
            }
        }
        Expr::Subscript { value, index } => {
            walk_expr_uses(value, u);
            walk_expr_uses(index, u);
        }
        Expr::Slice { lower, upper, step } => {
            for e in [lower, upper, step].into_iter().flatten() {
                walk_expr_uses(e, u);
            }
        }
        Expr::List(es) | Expr::Tuple(es) | Expr::Set(es) => {
            for e in es {
                walk_expr_uses(e, u);
            }
        }
        Expr::Dict(pairs) => {
            for (k, v) in pairs {
                walk_expr_uses(k, u);
                walk_expr_uses(v, u);
            }
        }
        Expr::ListComp { element, clauses }
        | Expr::SetComp { element, clauses } => {
            if comprehension_hoists(&[element], clauses) {
                if let Some(first) = clauses.first() {
                    walk_expr_uses(&first.iterable, u);
                }
                u.child_free.extend(comp_free_uses(&[element], clauses));
            } else {
                walk_expr_uses(element, u);
                for c in clauses {
                    walk_expr_uses(&c.iterable, u);
                    for cond in &c.conditions {
                        walk_expr_uses(cond, u);
                    }
                }
            }
        }
        Expr::GeneratorExp { element, clauses } => {
            if let Some(first) = clauses.first() {
                walk_expr_uses(&first.iterable, u);
            }
            let gen_body = build_genexpr_body(element, clauses);
            let refs: Vec<&Stmt> = gen_body.iter().collect();
            u.child_free
                .extend(func_free_uses(&[genexpr_param()], &refs));
        }
        Expr::DictComp { key, value, clauses } => {
            if comprehension_hoists(&[key, value], clauses) {
                if let Some(first) = clauses.first() {
                    walk_expr_uses(&first.iterable, u);
                }
                u.child_free.extend(comp_free_uses(&[key, value], clauses));
            } else {
                walk_expr_uses(key, u);
                walk_expr_uses(value, u);
                for c in clauses {
                    walk_expr_uses(&c.iterable, u);
                    for cond in &c.conditions {
                        walk_expr_uses(cond, u);
                    }
                }
            }
        }
        Expr::Walrus { value, .. } => {
            u.has_walrus = true;
            walk_expr_uses(value, u);
        }
        Expr::Yield(value) => {
            if let Some(v) = value {
                walk_expr_uses(v, u);
            }
        }
        Expr::YieldFrom(value) => walk_expr_uses(value, u),
    }
}

/// Infer `int`/`float` for unannotated locals whose every value-assignment is statically numeric (so
/// `x = 5` needs no `: int`, `s = 0.0` no `: float`). TWO monotone passes over the {Int, Float, Dynamic}
/// lattice, both from an `Int` seed: pass 1 WIDENS (an int/float mix promotes to `Float`), pass 2 then
/// DEMOTES a genuinely multi-typed local (a real int source AND a real float source) back to `Dynamic`.
/// So a chain like `a = 0; b = a; c = obj.x; a = c` settles, AND a float accumulator `s = 0.0; for v in
/// xs: s = s + v` settles `Float` -- the widen pass promotes its self-referential `s + v` while it is
/// transiently `Int` (the for-target `v` still settling), where a single non-promoting pass would
/// lock `s` at that transient mix (a sticky `Dynamic`). Parameters and annotated locals are pinned.
fn infer_local_types(
    params: &[ast::ParamDef],
    body: &[&Stmt],
    names: &[String],
    types: &mut [bc::StaticType],
    func_rets: &BTreeMap<String, bc::StaticType>,
) {
    let mut pinned = vec![false; names.len()];
    for p in pinned.iter_mut().take(params.len()) {
        *p = true;
    }
    let mut rhss: Vec<Vec<Expr>> = vec![Vec::new(); names.len()];
    for stmt in body {
        gather_assignments_stmt(stmt, names, &mut pinned, &mut rhss);
    }
    settle_numeric_types(names, &pinned, &rhss, types, func_rets);
    if settle_growable_lists(body, names, &mut pinned, &rhss, types, func_rets) {
        settle_numeric_types(names, &pinned, &rhss, types, func_rets);
    }
}

/// Seed every unpinned local optimistically `Int` and settle the numeric fixpoint over `types`.
///
/// Seeding `Int` (not `Dynamic`) is what lets a local whose int-ness DEPENDS on float locals -- e.g. a
/// compare result `flag = fa > fb` -- settle `Int` once those locals settle `Float`. The two passes
/// then run in order:
///
/// 1. WIDEN: settle each local to its widest numeric type, an int/float mix promoting to `Float` rather
///    than collapsing to a sticky `Dynamic` (so a float accumulator converges to `Float` even while its
///    self-referential `s + v` is transiently `Int`).
/// 2. DEMOTE: with the widened types as the environment, a local whose RHSs do NOT all agree (a genuine
///    int-and-float mix like `x = 1; x = 2.0`, or any non-numeric RHS) cannot be one machine width ->
///    `Dynamic`. Iterated inside [`settle_local_types`], so a demotion propagates to dependents. This
///    does NOT re-fire on the float accumulator: in the widened env its `s + v` is `Float`, agreeing
///    with the `0.0` seed.
fn settle_numeric_types(
    names: &[String],
    pinned: &[bool],
    rhss: &[Vec<Expr>],
    types: &mut [bc::StaticType],
    func_rets: &BTreeMap<String, bc::StaticType>,
) {
    for (i, ty) in types.iter_mut().enumerate() {
        if !pinned[i] {
            *ty = bc::StaticType::Int;
        }
    }
    settle_local_types(names, pinned, rhss, types, func_rets, promoted_kind);
    settle_local_types(names, pinned, rhss, types, func_rets, numeric_kind);
}

/// Reclassify every list local the function `append`s to as a GROWABLE list -- the header-and-backing
/// representation, since a fixed array cannot take a new element. Runs after the numeric passes, whose
/// settled types tell a seeded list's element kind.
///
/// The element kind comes from the seed literal (`xs = [1]` is already `ListInt`) or, for an
/// all-`[]` local that has no element to read, from what the appends push. Anything the two disagree
/// on is a heterogeneous list -- legal Python the typed lane has no representation for -- so it is
/// demoted to `Dynamic` and the function falls to the interpreter.
///
/// Every local this decides is PINNED, and whether it decided any is the return value (the caller
/// re-settles the numeric passes when so).
fn settle_growable_lists(
    body: &[&Stmt],
    names: &[String],
    pinned: &mut [bool],
    rhss: &[Vec<Expr>],
    types: &mut [bc::StaticType],
    func_rets: &BTreeMap<String, bc::StaticType>,
) -> bool {
    use bc::StaticType::{Dynamic, Float, GrowListFloat, GrowListInt, Int, ListFloat, ListInt};
    let mut appends: Vec<Vec<Expr>> = vec![Vec::new(); names.len()];
    for stmt in body {
        gather_appends_stmt(stmt, names, &mut appends);
    }
    let mut decided = false;
    for i in 0..names.len() {
        if pinned[i] || appends[i].is_empty() {
            continue;
        }
        let seeded_empty = !rhss[i].is_empty()
            && rhss[i]
                .iter()
                .all(|e| matches!(e, Expr::List(elems) if elems.is_empty()));
        let mut elem = match types[i] {
            ListInt => Some(Int),
            ListFloat => Some(Float),
            Dynamic if seeded_empty => None,
            _ => continue,
        };
        for arg in &appends[i] {
            let pushed = expr_static_type(arg, names, types, func_rets);
            match elem {
                None if matches!(pushed, Int | Float) => elem = Some(pushed),
                Some(kind) if kind == pushed => {}
                _ => {
                    elem = None;
                    break;
                }
            }
        }
        types[i] = match elem {
            Some(Int) => GrowListInt,
            Some(Float) => GrowListFloat,
            _ => Dynamic,
        };
        pinned[i] = true;
        decided = true;
    }
    decided
}

/// Collect the argument of every statement-level `<local>.append(<arg>)` in `stmt`, per local.
///
/// An `append` in any other position (its `None` result used as a value) is not gathered: the local
/// then stays fixed and the lowering refuses the `append`, so the function falls to the interpreter
/// rather than growing a list the analysis never saw.
fn gather_appends_stmt(stmt: &Stmt, names: &[String], appends: &mut [Vec<Expr>]) {
    match stmt {
        Stmt::Expr(Expr::Call { func, args, keywords }) if keywords.is_empty() && args.len() == 1 => {
            if let Expr::Attribute { value, attr } = &**func {
                if attr == "append" {
                    if let Expr::Name(name) = &**value {
                        if let Some(slot) = names.iter().position(|n| n == name) {
                            appends[slot].push(args[0].clone());
                        }
                    }
                }
            }
        }
        Stmt::If { body, orelse, .. } => {
            for s in body.iter().chain(orelse) {
                gather_appends_stmt(s, names, appends);
            }
        }
        Stmt::While { body, orelse, .. } => {
            for s in body.iter().chain(orelse) {
                gather_appends_stmt(s, names, appends);
            }
        }
        Stmt::For { body, orelse, .. } | Stmt::ForIter { body, orelse, .. } => {
            for s in body.iter().chain(orelse) {
                gather_appends_stmt(s, names, appends);
            }
        }
        Stmt::With { body, .. } => {
            for s in body {
                gather_appends_stmt(s, names, appends);
            }
        }
        Stmt::Try { body, handlers, orelse, finalbody } => {
            for s in body.iter().chain(orelse).chain(finalbody) {
                gather_appends_stmt(s, names, appends);
            }
            for handler in handlers {
                for s in &handler.body {
                    gather_appends_stmt(s, names, appends);
                }
            }
        }
        _ => {}
    }
}

/// How a settle pass reads a local's type out of its assignment RHSs -- `promoted_kind` (widen) or
/// `numeric_kind` (demote). See [`settle_numeric_types`].
type KindFn = fn(&[Expr], &[String], &[bc::StaticType], &BTreeMap<String, bc::StaticType>) -> bc::StaticType;

/// Run one monotone fixpoint of `kind` over the unpinned locals, updating `types` in place until no
/// local's type changes. `kind` maps a local's RHSs (read against the current `types`) to its settled
/// type; a RHS may name another local, so iterate. Shared by the widen and demote passes of
/// [`infer_local_types`].
fn settle_local_types(
    names: &[String],
    pinned: &[bool],
    rhss: &[Vec<Expr>],
    types: &mut [bc::StaticType],
    func_rets: &BTreeMap<String, bc::StaticType>,
    kind: KindFn,
) {
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..names.len() {
            if pinned[i] {
                continue;
            }
            let new_ty = kind(&rhss[i], names, types, func_rets);
            if new_ty != types[i] {
                types[i] = new_ty;
                changed = true;
            }
        }
    }
}

/// The common numeric static type of a local's assignment RHSs -- pass 2 (DEMOTE) of
/// [`infer_local_types`]: `Int` if every RHS is provably Int, `Float` if every RHS is provably Float,
/// else `Dynamic` -- no RHS, any dynamic RHS, or a MIXED int/float local (one MIR slot cannot be both
/// machine widths). Run against the widened environment from pass 1, so a float accumulator's `s + v`
/// reads Float (agreeing with its `0.0` seed) while a genuine `x = 1; x = 2.0` reads a real int and a
/// real float and is correctly demoted.
fn numeric_kind(
    rhss: &[Expr],
    names: &[String],
    types: &[bc::StaticType],
    func_rets: &BTreeMap<String, bc::StaticType>,
) -> bc::StaticType {
    let mut kind: Option<bc::StaticType> = None;
    for e in rhss {
        let t = expr_static_type(e, names, types, func_rets);
        match (kind, t) {
            (_, bc::StaticType::Dynamic) => return bc::StaticType::Dynamic,
            (None, t) => kind = Some(t),
            (Some(k), t) if k == t => {}
            (Some(_), _) => return bc::StaticType::Dynamic,
        }
    }
    kind.unwrap_or(bc::StaticType::Dynamic)
}

/// The WIDEST numeric type over a local's RHSs -- pass 1 (WIDEN) of [`infer_local_types`]: like
/// [`numeric_kind`] but an int/float MIX promotes to `Float` instead of collapsing to `Dynamic`. This
/// settles a float accumulator (`s = 0.0; s = s + v`) to `Float` even while its self-referential
/// `s + v` is transiently `Int` (the for-target `v` not yet settled) -- the sticky-`Dynamic`
/// self-poisoning a single non-promoting pass would hit. Only the two scalar numerics promote; a
/// container vs a scalar, or two different container element kinds, is still `Dynamic`. Pass 2 then
/// demotes any GENUINE int/float mix.
fn promoted_kind(
    rhss: &[Expr],
    names: &[String],
    types: &[bc::StaticType],
    func_rets: &BTreeMap<String, bc::StaticType>,
) -> bc::StaticType {
    use bc::StaticType::{Dynamic, Float, Int};
    let mut kind: Option<bc::StaticType> = None;
    for e in rhss {
        let t = expr_static_type(e, names, types, func_rets);
        kind = Some(match (kind, t) {
            (_, Dynamic) => return Dynamic,
            (None, t) => t,
            (Some(a), b) if a == b => a,
            (Some(Int | Float), Int | Float) => Float,
            (Some(_), _) => return Dynamic,
        });
    }
    kind.unwrap_or(Dynamic)
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
                let mut bound = Vec::new();
                target.collect_names(&mut bound);
                for name in bound {
                    if let Some(slot) = names.iter().position(|n| n == name) {
                        rhss[slot].push(value.clone());
                    }
                }
            }
        }
        Stmt::TupleAssign { targets, star, value } => {
            let all_names = star.is_none()
                && targets
                    .iter()
                    .all(|t| matches!(t, ast::AssignTarget::Name(_)));
            let decomposed: Option<Vec<Expr>> = if !all_names {
                None
            } else if let Expr::Tuple(elems) = value {
                (elems.len() == targets.len()).then(|| elems.clone())
            } else if let Expr::Call { func, args, keywords } = value {
                (keywords.is_empty()
                    && args.len() == 2
                    && targets.len() == 2
                    && matches!(&**func, Expr::Name(n) if n == "divmod"))
                .then(|| {
                    let binary = |op| Expr::Binary {
                        op,
                        lhs: Box::new(args[0].clone()),
                        rhs: Box::new(args[1].clone()),
                    };
                    vec![binary(ast::BinOp::FloorDiv), binary(ast::BinOp::Mod)]
                })
            } else {
                None
            };
            if let Some(exprs) = decomposed {
                for (target, expr) in targets.iter().zip(exprs) {
                    if let ast::AssignTarget::Name(name) = target {
                        if let Some(slot) = names.iter().position(|n| n == name) {
                            rhss[slot].push(expr);
                        }
                    }
                }
            } else {
                for target in targets {
                    let mut bound = Vec::new();
                    target.collect_names(&mut bound);
                    for name in bound {
                        if let Some(slot) = names.iter().position(|n| n == name) {
                            pinned[slot] = true;
                        }
                    }
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
            iterable,
            body,
            orelse,
            ..
        } => {
            if let Some(slot) = names.iter().position(|n| n == target) {
                rhss[slot].push(Expr::Subscript {
                    value: Box::new(iterable.clone()),
                    index: Box::new(Expr::Int(0)),
                });
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
        Stmt::With {
            optional_name,
            body,
            ..
        } => {
            if let Some(name) = optional_name {
                if let Some(slot) = names.iter().position(|n| n == name) {
                    pinned[slot] = true;
                }
            }
            for s in body {
                gather_assignments_stmt(s, names, pinned, rhss);
            }
        }
        Stmt::ClassDef { name, .. } => {
            if let Some(slot) = names.iter().position(|n| n == name) {
                pinned[slot] = true;
            }
        }
        Stmt::Return(_)
        | Stmt::Expr(_)
        | Stmt::Delete(_)
        | Stmt::Nonlocal(_)
        | Stmt::Global(_)
        | Stmt::FuncDef(_)
        | Stmt::Decorated { .. }
        | Stmt::Import { .. }
        | Stmt::ImportFrom { .. }
        | Stmt::ImportStar { .. }
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Pass
        | Stmt::SetItem { .. }
        | Stmt::SetAttr { .. }
        | Stmt::Raise { .. } => {}
    }
}

/// The statically-known type of an expression given the locals settled so far, MATCHING what the
/// typed lowering emits: an integer/boolean literal or integer arithmetic is `Int`; a float literal,
/// `/` (true division, always float), or a `+ - *` with a float operand is `Float` (Python promotes);
/// a comparison of two numerics is `Int` (a 0/1 bool); a call result, attribute, `None`, or string is
/// `Dynamic`. Kept in lockstep with `lower.rs` so an inferred slot type always matches the value the
/// lowering stores into it.
fn expr_static_type(
    expr: &Expr,
    names: &[String],
    types: &[bc::StaticType],
    func_rets: &BTreeMap<String, bc::StaticType>,
) -> bc::StaticType {
    use bc::StaticType::{
        Dynamic, Float, GrowListFloat, GrowListInt, Int, ListFloat, ListInt, TupleFloat, TupleInt,
    };
    fn numeric(t: bc::StaticType) -> bool {
        matches!(t, Int | Float)
    }
    match expr {
        Expr::Int(_) | Expr::Bool(_) => Int,
        Expr::Float(_) => Float,
        Expr::Name(n) => names
            .iter()
            .position(|x| x == n)
            .map(|i| types[i])
            .unwrap_or(Dynamic),
        Expr::Binary { op, lhs, rhs } | Expr::InplaceBinary { op, lhs, rhs } => {
            let a = expr_static_type(lhs, names, types, func_rets);
            let b = expr_static_type(rhs, names, types, func_rets);
            if !numeric(a) || !numeric(b) {
                return Dynamic;
            }
            match op {
                ast::BinOp::Add | ast::BinOp::Sub | ast::BinOp::Mul => {
                    if a == Int && b == Int { Int } else { Float }
                }
                ast::BinOp::TrueDiv => Float,
                ast::BinOp::FloorDiv
                | ast::BinOp::Mod
                | ast::BinOp::BitAnd
                | ast::BinOp::BitOr
                | ast::BinOp::BitXor
                | ast::BinOp::LShift
                | ast::BinOp::RShift => {
                    if a == Int && b == Int { Int } else { Dynamic }
                }
                ast::BinOp::Pow | ast::BinOp::MatMul => Dynamic,
            }
        }
        Expr::Compare { lhs, rhs, .. } => {
            if numeric(expr_static_type(lhs, names, types, func_rets))
                && numeric(expr_static_type(rhs, names, types, func_rets))
            {
                Int
            } else {
                Dynamic
            }
        }
        Expr::BoolBinary { lhs, rhs, .. } => {
            if expr_static_type(lhs, names, types, func_rets) == Int
                && expr_static_type(rhs, names, types, func_rets) == Int
            {
                Int
            } else {
                Dynamic
            }
        }
        Expr::Unary { op, operand } => {
            let t = expr_static_type(operand, names, types, func_rets);
            match op {
                ast::UnaryOp::Invert => {
                    if t == Int { Int } else { Dynamic }
                }
                ast::UnaryOp::Neg | ast::UnaryOp::Pos => {
                    if numeric(t) { t } else { Dynamic }
                }
            }
        }
        Expr::Not { .. } => Int,
        Expr::Conditional { body, orelse, .. } => {
            let a = expr_static_type(body, names, types, func_rets);
            let b = expr_static_type(orelse, names, types, func_rets);
            if a == b && numeric(a) { a } else { Dynamic }
        }
        Expr::Call { func, args, keywords }
            if keywords.is_empty()
                && args.len() == 1
                && matches!(&**func, Expr::Name(n) if n == "int" || n == "float" || n == "bool")
                && numeric(expr_static_type(&args[0], names, types, func_rets)) =>
        {
            match &**func {
                Expr::Name(n) if n == "float" => Float,
                _ => Int,
            }
        }
        Expr::Call { func, args, keywords }
            if keywords.is_empty()
                && args.len() == 1
                && matches!(&**func, Expr::Name(n) if n == "abs")
                && numeric(expr_static_type(&args[0], names, types, func_rets)) =>
        {
            expr_static_type(&args[0], names, types, func_rets)
        }
        Expr::Call { func, args, keywords }
            if keywords.is_empty()
                && args.len() == 1
                && matches!(&**func, Expr::Name(n) if n == "round")
                && numeric(expr_static_type(&args[0], names, types, func_rets)) =>
        {
            Int
        }
        Expr::Call { func, args, keywords }
            if keywords.is_empty()
                && args.len() == 2
                && matches!(&**func, Expr::Name(n) if n == "min" || n == "max") =>
        {
            let a = expr_static_type(&args[0], names, types, func_rets);
            let b = expr_static_type(&args[1], names, types, func_rets);
            if a == b && numeric(a) { a } else { Dynamic }
        }
        Expr::Call { func, args, keywords }
            if keywords.is_empty()
                && matches!(&**func, Expr::Name(n) if matches!(n.as_str(),
                    "mmio_read8" | "mmio_read16" | "mmio_read32"))
                && args.iter().all(|a| expr_static_type(a, names, types, func_rets) == Int) =>
        {
            Int
        }
        Expr::Call { func, args, keywords }
            if keywords.is_empty()
                && args.len() == 1
                && matches!(&**func, Expr::Name(n) if n == "len") =>
        {
            Int
        }
        Expr::Call { func, .. } => match &**func {
            Expr::Name(name) if !names.contains(name) => {
                func_rets.get(name).copied().unwrap_or(Dynamic)
            }
            _ => Dynamic,
        },
        Expr::List(elems) if !elems.is_empty() => {
            let first = expr_static_type(&elems[0], names, types, func_rets);
            let container = match first {
                Int => ListInt,
                Float => ListFloat,
                _ => return Dynamic,
            };
            if elems[1..].iter().all(|e| expr_static_type(e, names, types, func_rets) == first) {
                container
            } else {
                Dynamic
            }
        }
        Expr::Tuple(elems) if !elems.is_empty() => {
            let first = expr_static_type(&elems[0], names, types, func_rets);
            let container = match first {
                Int => TupleInt,
                Float => TupleFloat,
                _ => return Dynamic,
            };
            if elems[1..].iter().all(|e| expr_static_type(e, names, types, func_rets) == first) {
                container
            } else {
                Dynamic
            }
        }
        Expr::Subscript { value, index } => {
            if expr_static_type(index, names, types, func_rets) != Int {
                return Dynamic;
            }
            match expr_static_type(value, names, types, func_rets) {
                ListInt | TupleInt | GrowListInt => Int,
                ListFloat | TupleFloat | GrowListFloat => Float,
                _ => Dynamic,
            }
        }
        _ => Dynamic,
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
    /// The locals promoted to heap cells because a nested function captures them (deref indices
    /// `0..cellvars.len()`). Reads/writes of these names emit `LoadDeref`/`StoreDeref`, not Fast.
    cellvars: Vec<String>,
    /// The names captured from an enclosing function (deref indices continue after `cellvars`).
    /// Reads emit `LoadDeref`; a nested function's def site loads them with `LoadClosure`.
    freevars: Vec<String>,
    /// The names declared `global` in this scope. A read emits `LoadGlobal` and a write `StoreGlobal`
    /// (the module namespace), never a local slot or a cell -- these names were excluded from
    /// `local_names` / `cellvars` / `freevars`.
    globals: BTreeSet<String>,
    /// The scope chain a nested function of this one sees -- the enclosing FUNCTION scopes' bound
    /// names plus this function's own -- so a nested function can tell a captured free variable
    /// (bound in some entry here) from a global.
    child_scopes: Vec<BTreeSet<String>>,
    /// A stack of the enclosing loops' `(continue, break, finally_depth, handler_depth)`: the jump
    /// targets plus `self.finallys.len()` and `self.handler_depth` at loop entry, so a
    /// break/continue re-emits only the `finally` bodies -- and clears only the `except` handlers --
    /// entered inside that loop.
    loops: Vec<(Label, Label, usize, usize)>,
    /// A stack of active `finally` bodies (innermost last). An exit -- fall-through, return,
    /// break, continue, or the exception copy -- re-emits the crossed bodies (the duplication
    /// model). The bodies are stack-neutral, so a held return value survives across them.
    finallys: Vec<Vec<Stmt>>,
    /// How many `except` HANDLER bodies enclose the current point. The runtime keeps the exception
    /// being handled in a single per-frame slot (cleared by `PopExcept` at the handler's end), so a
    /// `break`/`continue` that leaves a handler must clear that slot too -- otherwise a later bare
    /// `raise` would re-raise a stale, already-handled exception.
    handler_depth: usize,
    /// True while compiling the statements directly in a `class` body (not a nested def/lambda),
    /// so a bare-name read emits `LoadName` (namespace -> global -> built-in) instead of
    /// `LoadGlobal` -- letting a member read a name the body bound earlier.
    in_class_body: bool,
    /// The names bound in the class body currently being emitted (member assignments + method
    /// names); empty outside a class body. A read of one of these resolves namespace-first
    /// (`LoadName`), NOT as an enclosing function local/cell -- so a class attribute that shadows an
    /// enclosing or module name reads the class attribute, matching CPython's class scope.
    class_body_bound: BTreeSet<String>,
    /// The enclosing class name, so `super()` in a method resolves to its base.
    current_class: Option<String>,
    /// This code object's own name, used to prefix hoisted-lambda names for uniqueness.
    name: String,
    /// Lambda functions hoisted out of this code object (and nested ones), appended to the
    /// module's function table so their `MakeFunction` references resolve.
    hoisted: Vec<bc::CodeObject>,
    /// Counts hoisted lambdas within this code object, for unique naming.
    lambda_counter: usize,
    /// Set when the body emits a `Yield`, marking this code object a generator function.
    has_yield: bool,
    /// True while compiling a DIRECT statement of this code object's body (set per statement by the
    /// body loop, cleared on entry to `compile_stmt` so a nested statement sees false). At module
    /// scope this distinguishes a direct top-level `def` -- a pure function-table entry already
    /// hoisted by `compile_module` -- from one buried in an `if`/`for`/`while`/`try`/`with` body,
    /// which must be hoisted + emitted at its def site (a version-guarded / fallback def).
    direct_body_stmt: bool,
    /// Set by `compile_decorated` around the def it wraps, and consumed by `compile_stmt` like
    /// `direct_body_stmt`: a DECORATED def never owns its bare name -- the decorator's RESULT is bound
    /// to it -- so its body is hoisted under a synthetic name and the bare name is left for the wrapping
    /// StoreName, exactly as a superseded def is. Otherwise the raw body would sit in the function table
    /// under the bare name and a `LoadGlobal` of it would call the UNDECORATED body.
    decorating_a_def: bool,
    /// Counts block-nested module defs hoisted from this body, for a unique table name per def site
    /// (so sibling same-named defs -- `if c: def f else: def f` -- do not collide on one name).
    block_def_counter: usize,
    /// Per-name occurrence count of a nested `def` hoisted from this (function) body, so sibling
    /// same-named defs get DISTINCT code-object names (`scope.name`, then `scope.name$1`, ...). The
    /// first keeps the bare qualified name (stable), so only a redefinition is suffixed.
    nested_def_counts: BTreeMap<String, usize>,
    /// How many times each name is `def`ed directly in THIS body, against how many of those have been
    /// compiled so far -- module scope only. Together they say whether the def being compiled is the
    /// LAST of its name, and so the one that owns the function-table entry under that bare name.
    module_def_totals: BTreeMap<String, usize>,
    module_defs_seen: BTreeMap<String, usize>,
    /// The module functions this scope may resolve a call's type against -- their DECLARED return
    /// types. Empty for a scope nested in a function, which could capture one of those names.
    func_rets: BTreeMap<String, bc::StaticType>,
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

    /// The deref index for a cell or free variable of this function, if `name` is one. Cellvars
    /// occupy `0..cellvars.len()`, then freevars follow -- one index space, so `LoadDeref` /
    /// `StoreDeref` reach both this frame's own cells and the cells captured from an enclosing one.
    fn deref_slot(&self, name: &str) -> Option<u32> {
        if let Some(i) = self.cellvars.iter().position(|n| n == name) {
            return Some(i as u32);
        }
        self.freevars
            .iter()
            .position(|n| n == name)
            .map(|i| (self.cellvars.len() + i) as u32)
    }

    /// Emit a read of `name`: a class-local (a name bound in the enclosing class body) through
    /// `LoadName` (namespace -> global -> built-in); else a cell/free variable through `LoadDeref`, a
    /// plain local through `LoadFast`, a module global through `LoadGlobal`, or -- for a NON-class-local
    /// name read directly in a class body -- `LoadName`. A nested def/lambda inside the body compiles
    /// in its own scope (not `in_class_body`), so its reads stay Global/Fast/Deref, matching CPython.
    fn emit_load_name(&mut self, name: &str) {
        if self.globals.contains(name) {
            let idx = self.name_index(name);
            self.asm.emit(bc::Op::LoadGlobal(idx));
            return;
        }
        if self.in_class_body && self.class_body_bound.contains(name) {
            let idx = self.name_index(name);
            self.asm.emit(bc::Op::LoadName(idx));
            return;
        }
        if let Some(deref) = self.deref_slot(name) {
            self.asm.emit(bc::Op::LoadDeref(deref));
        } else if let Some(slot) = self.local_slot(name) {
            self.asm.emit(bc::Op::LoadFast(slot));
        } else {
            let idx = self.name_index(name);
            if self.in_class_body {
                self.asm.emit(bc::Op::LoadName(idx));
            } else {
                self.asm.emit(bc::Op::LoadGlobal(idx));
            }
        }
    }

    /// Emit a store to `name`: a cell variable through `StoreDeref`, otherwise `StoreFast` to its
    /// local slot (every bound name has one, from the pre-pass; a module-level store set_globals it).
    fn emit_store_name(&mut self, name: &str) {
        if self.globals.contains(name) {
            let idx = self.name_index(name);
            self.asm.emit(bc::Op::StoreGlobal(idx));
            return;
        }
        if let Some(deref) = self.deref_slot(name) {
            self.asm.emit(bc::Op::StoreDeref(deref));
        } else {
            let slot = self
                .local_slot(name)
                .expect("an assigned name is always a local (added by the pre-pass)");
            self.asm.emit(bc::Op::StoreFast(slot));
        }
    }

    /// Emit one `LoadClosure` per free variable of a nested function (in that function's freevar
    /// order), pushing the matching cell from this frame's deref array. Returns the `CLOSURE` flag
    /// bit for `MakeFunction` (0 when the nested function captures nothing).
    fn emit_captured_cells(&mut self, freevars: &[String]) -> u8 {
        if freevars.is_empty() {
            return 0;
        }
        for fv in freevars {
            let deref = self
                .deref_slot(fv)
                .expect("a nested function's freevar is a cell or free variable of this one");
            self.asm.emit(bc::Op::LoadClosure(deref));
        }
        0x04
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), CompileError> {
        let direct = self.direct_body_stmt;
        self.direct_body_stmt = false;
        let decorated = self.decorating_a_def;
        self.decorating_a_def = false;
        match stmt {
            Stmt::FuncDef(func) => {
                if self.scope == Scope::Function {
                    self.compile_nested_def(func)
                } else if direct && self.owns_module_def_name(&func.name) && !decorated {
                    self.compile_module_def(func)
                } else {
                    self.compile_hoisted_module_def(func)
                }
            }
            Stmt::Delete(targets) => {
                for target in targets {
                    match target {
                        Expr::Name(name) => {
                            let slot = self.local_slot(name).ok_or_else(|| {
                                error("cannot delete a name that is not a local in this scope")
                            })?;
                            self.asm.emit(bc::Op::DeleteFast(slot));
                        }
                        Expr::Subscript { value, index } => {
                            self.compile_expr(value)?;
                            self.compile_expr(index)?;
                            self.asm.emit(bc::Op::DeleteItem);
                        }
                        Expr::Attribute { value, attr } => {
                            self.compile_expr(value)?;
                            let name = self.name_index(attr);
                            self.asm.emit(bc::Op::DeleteAttr { name });
                        }
                        _ => {
                            return Err(error(
                                "a del target must be a name, a subscript, or an attribute",
                            ));
                        }
                    }
                }
                Ok(())
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
            Stmt::Decorated { decorators, inner } => self.compile_decorated(decorators, inner, direct),
            Stmt::MultiAssign { targets, value } => self.compile_multi_assign(targets, value),
            Stmt::TupleAssign { targets, star, value } => {
                self.compile_expr(value)?;
                match star {
                    None => self.asm.emit(bc::Op::UnpackSequence(targets.len() as u32)),
                    Some(i) => self.asm.emit(bc::Op::UnpackEx {
                        before: *i as u32,
                        after: (targets.len() - 1 - i) as u32,
                    }),
                }
                for target in targets {
                    self.compile_unpack_target(target)?;
                }
                Ok(())
            }
            Stmt::SetItem {
                container,
                index,
                value,
                op,
            } => self.compile_setitem(container, index, value, *op),
            Stmt::SetAttr {
                obj,
                attr,
                value,
                op,
            } => self.compile_setattr(obj, attr, value, *op),
            Stmt::ClassDef { name, bases, body } => self.compile_classdef(name, bases, body, direct),
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
            Stmt::With {
                context,
                optional_name,
                body,
            } => self.compile_with(context, optional_name, body),
            Stmt::Break => {
                let (_, target, fin_depth, handler_depth) = self
                    .loops
                    .last()
                    .copied()
                    .ok_or_else(|| error("'break' outside a loop"))?;
                self.emit_finallys_from(fin_depth)?;
                self.emit_pop_handlers_to(handler_depth);
                self.asm.emit_jump(target);
                Ok(())
            }
            Stmt::Continue => {
                let (target, _, fin_depth, handler_depth) = self
                    .loops
                    .last()
                    .copied()
                    .ok_or_else(|| error("'continue' outside a loop"))?;
                self.emit_finallys_from(fin_depth)?;
                self.emit_pop_handlers_to(handler_depth);
                self.asm.emit_jump(target);
                Ok(())
            }
            Stmt::Import { modules } => {
                for (module, bound) in modules {
                    let midx = self.name_index(module);
                    self.asm.emit(bc::Op::ImportName(midx));
                    self.emit_store_name(bound);
                }
                Ok(())
            }
            Stmt::ImportFrom { module, names } => {
                let midx = self.name_index(module);
                self.asm.emit(bc::Op::ImportName(midx));
                for (member, bound) in names {
                    let nidx = self.name_index(member);
                    self.asm.emit(bc::Op::ImportFrom(nidx));
                    self.emit_store_name(bound);
                }
                self.asm.emit(bc::Op::PopTop);
                Ok(())
            }
            Stmt::ImportStar { module } => {
                if self.scope != Scope::Module {
                    return Err(error("import * is only allowed at module level"));
                }
                let midx = self.name_index(module);
                self.asm.emit(bc::Op::ImportName(midx));
                self.asm.emit(bc::Op::ImportStar);
                Ok(())
            }
            Stmt::Nonlocal(_) | Stmt::Global(_) => Ok(()),
            Stmt::Pass => Ok(()),
        }
    }

    /// Compile a decorated `def`/`class`: define it (binding `name`), then rebind
    /// `name = d0(d1(... dn(name) ...))` -- the decorators wrap the name bottom-up (the one nearest
    /// the `def` applied first). Each `decorator(name)` is an ordinary call.
    fn compile_decorated(
        &mut self,
        decorators: &[Expr],
        inner: &Stmt,
        direct: bool,
    ) -> Result<(), CompileError> {
        self.direct_body_stmt = direct;
        self.decorating_a_def = matches!(inner, Stmt::FuncDef(_));
        self.compile_stmt(inner)?;
        let name = match inner {
            Stmt::FuncDef(f) => f.name.clone(),
            Stmt::ClassDef { name, .. } => name.clone(),
            _ => unreachable!("the parser wraps only a def or class"),
        };
        let mut value = Expr::Name(name.clone());
        for decorator in decorators.iter().rev() {
            value = Expr::Call {
                func: Box::new(decorator.clone()),
                args: vec![value],
                keywords: Vec::new(),
            };
        }
        self.compile_assign(&Assign {
            target: name,
            annotation: None,
            value: Some(value),
        })
    }

    fn compile_assign(&mut self, assign: &Assign) -> Result<(), CompileError> {
        let Some(value) = &assign.value else {
            return Ok(());
        };
        self.compile_expr(value)?;
        self.emit_store_name(&assign.target);
        Ok(())
    }

    /// `a = b = value`: evaluate the value once, store it to the first target, then copy
    /// that into each remaining target (left to right), so all bind the same value.
    fn compile_multi_assign(
        &mut self,
        targets: &[ast::AssignTarget],
        value: &Expr,
    ) -> Result<(), CompileError> {
        self.compile_expr(value)?;
        let temp = self.alloc_temp();
        self.asm.emit(bc::Op::StoreFast(temp));
        for target in targets {
            self.asm.emit(bc::Op::LoadFast(temp));
            self.compile_unpack_target(target)?;
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
        self.loops.push((top_label, after_label, self.finallys.len(), self.handler_depth));
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
        self.emit_store_name(target);
        self.loops.push((cont, after, self.finallys.len(), self.handler_depth));
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
                self.emit_store_name(name);
            }
            self.handler_depth += 1;
            for stmt in &handler.body {
                self.compile_stmt(stmt)?;
            }
            self.handler_depth -= 1;
            if let Some(name) = &handler.name {
                let slot = self
                    .local_slot(name)
                    .expect("the except-clause name is a local");
                self.asm.emit(bc::Op::DeleteFast(slot));
            }
            self.asm.emit(bc::Op::PopExcept);
            self.asm.emit_jump(after);
            self.asm.place(next);
        }
        self.asm.emit(bc::Op::Reraise);
        self.asm.place(after);
        Ok(())
    }

    /// Clear the exception slot for each `except` handler crossed by a `break`/`continue` that
    /// leaves it -- from `self.handler_depth` down to `target_depth` (the loop's handler depth at
    /// entry). The runtime keeps the handled exception in one per-frame slot, so a `PopExcept`
    /// clears it; leaving a handler this way must clear it, else a later bare `raise` would see a
    /// stale exception.
    fn emit_pop_handlers_to(&mut self, target_depth: usize) {
        for _ in target_depth..self.handler_depth {
            self.asm.emit(bc::Op::PopExcept);
        }
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

    /// Compile `with context [as name]: body` by desugaring to the full context-manager protocol
    /// (PEP 343) over a try/except/finally, so `__exit__` runs on every exit -- normal fall-through,
    /// return/break/continue, AND exception -- with the correct arguments and honouring suppression:
    /// ```text
    /// _mgr = context; [name =] _mgr.__enter__()
    /// _hit = False
    /// try:
    ///     body
    /// except <any> as _exc:            # catch-all that binds the exception value
    ///     _hit = True
    ///     if not _mgr.__exit__(type(_exc), _exc, None):
    ///         raise                    # __exit__ returned falsy -> propagate (a bare re-raise)
    /// finally:
    ///     if not _hit:
    ///         _mgr.__exit__(None, None, None)
    /// ```
    /// The `_hit` flag routes the exception exit (where `__exit__` already ran with the exception
    /// info) away from the finally's normal-exit `__exit__(None, None, None)`. A truthy `__exit__`
    /// return SUPPRESSES the exception (the handler falls through); a falsy one re-raises it. The
    /// traceback argument is `None` (no traceback objects in this subset). The manager, the flag, and
    /// the caught exception each live in a temp local.
    fn compile_with(
        &mut self,
        context: &Expr,
        optional_name: &Option<String>,
        body: &[Stmt],
    ) -> Result<(), CompileError> {
        let mgr = self.alloc_temp();
        let mgr_name = format!(".t{mgr}");
        self.compile_expr(context)?;
        self.asm.emit(bc::Op::StoreFast(mgr));
        let enter = Expr::Call {
            func: Box::new(Expr::Attribute {
                value: Box::new(Expr::Name(mgr_name.clone())),
                attr: String::from("__enter__"),
            }),
            args: Vec::new(),
            keywords: Vec::new(),
        };
        self.compile_expr(&enter)?;
        match optional_name {
            Some(name) => self.emit_store_name(name),
            None => self.asm.emit(bc::Op::PopTop),
        }
        let hit = self.alloc_temp();
        let hit_name = format!(".t{hit}");
        let exc = self.alloc_temp();
        let exc_name = format!(".t{exc}");
        self.compile_expr(&Expr::Bool(false))?;
        self.asm.emit(bc::Op::StoreFast(hit));

        let exit_call = |args: Vec<Expr>| Expr::Call {
            func: Box::new(Expr::Attribute {
                value: Box::new(Expr::Name(mgr_name.clone())),
                attr: String::from("__exit__"),
            }),
            args,
            keywords: Vec::new(),
        };
        let type_of_exc = Expr::Call {
            func: Box::new(Expr::Name(String::from("type"))),
            args: vec![Expr::Name(exc_name.clone())],
            keywords: Vec::new(),
        };
        let handler_exit = exit_call(vec![type_of_exc, Expr::Name(exc_name.clone()), Expr::None]);
        let handler = ExceptHandler {
            typ: None,
            name: Some(exc_name),
            body: vec![
                Stmt::Assign(Assign {
                    target: hit_name.clone(),
                    annotation: None,
                    value: Some(Expr::Bool(true)),
                }),
                Stmt::If {
                    test: Expr::Not {
                        operand: Box::new(handler_exit),
                    },
                    body: vec![Stmt::Raise {
                        exc: None,
                        cause: None,
                    }],
                    orelse: Vec::new(),
                },
            ],
        };
        let final_exit = exit_call(vec![Expr::None, Expr::None, Expr::None]);
        let finalbody = vec![Stmt::If {
            test: Expr::Not {
                operand: Box::new(Expr::Name(hit_name)),
            },
            body: vec![Stmt::Expr(final_exit)],
            orelse: Vec::new(),
        }];
        self.compile_try_finally(body, &[handler], &[], &finalbody)
    }

    /// `class Name [(Base)]:` -- push the name and base, `SetupClassNamespace` a fresh namespace,
    /// bind each member into it with `StoreName` (a method as `MakeFunction("Name.method")`, an
    /// attribute as its compiled value), `BuildClass` over the namespace register + `[name, base]`,
    /// and bind the class to its name. Member expressions are compiled with `in_class_body` set, so a
    /// bare-name read emits `LoadName` (namespace -> global -> built-in): a member can read a name the
    /// body bound earlier (`b = a + 1`, `x = property(get, set)`, `@radius.setter`). The base is
    /// evaluated BEFORE the namespace is set up, in the enclosing scope.
    fn compile_classdef(
        &mut self,
        name: &str,
        bases: &[Expr],
        body: &[Stmt],
        direct: bool,
    ) -> Result<(), CompileError> {
        let class_qual = if self.scope == Scope::Module {
            if direct {
                String::from(name)
            } else {
                let seq = self.block_def_counter;
                self.block_def_counter += 1;
                let qualified = format!("{}.${seq}.{}", self.name, name);
                compile_class_method_bodies(&qualified, body, &mut self.hoisted, &[], &self.func_rets.clone())?;
                qualified
            }
        } else {
            let qualified = format!("{}.{}", self.name, name);
            let enclosing = self.child_scopes.clone();
            compile_class_method_bodies(&qualified, body, &mut self.hoisted, &enclosing, &self.func_rets.clone())?;
            qualified
        };
        let name_const = self.const_index(bc::Const::Str(String::from(name)));
        self.asm.emit(bc::Op::LoadConst(name_const));
        match bases {
            [] => {
                let none = self.const_index(bc::Const::None);
                self.asm.emit(bc::Op::LoadConst(none));
            }
            [single] => self.compile_expr(single)?,
            many => {
                for b in many {
                    self.compile_expr(b)?;
                }
                self.asm.emit(bc::Op::BuildTuple(many.len() as u32));
            }
        }
        self.asm.emit(bc::Op::SetupClassNamespace);
        self.in_class_body = true;
        self.class_body_bound = class_body_bound_names(body);
        let names = method_qualified_names(&class_qual, body);
        for (i, member) in body.iter().enumerate() {
            match member {
                Stmt::FuncDef(method) => {
                    let qualified = names[i].as_ref().expect("a method has a qualified name");
                    let freevars = self.method_freevars(qualified);
                    let f = self.name_index(qualified);
                    let mut flags = self.emit_param_defaults(&method.params)?;
                    flags |= self.emit_captured_cells(&freevars);
                    self.asm.emit(bc::Op::MakeFunction { func: f, flags });
                    self.emit_store_member(&method.name);
                }
                Stmt::Decorated { decorators, inner } => {
                    let Stmt::FuncDef(method) = &**inner else {
                        self.in_class_body = false;
                        return Err(error("only a method may be decorated in a class body"));
                    };
                    for decorator in decorators {
                        self.compile_expr(decorator)?;
                    }
                    let qualified = names[i].as_ref().expect("a method has a qualified name");
                    let freevars = self.method_freevars(qualified);
                    let f = self.name_index(qualified);
                    let mut flags = self.emit_param_defaults(&method.params)?;
                    flags |= self.emit_captured_cells(&freevars);
                    self.asm.emit(bc::Op::MakeFunction { func: f, flags });
                    for _ in decorators {
                        self.asm.emit(bc::Op::Call(1));
                    }
                    self.emit_store_member(&method.name);
                }
                Stmt::Assign(assign) => {
                    if let Some(value) = &assign.value {
                        self.compile_expr(value)?;
                        self.emit_store_member(&assign.target);
                    }
                }
                _ => {}
            }
        }
        self.in_class_body = false;
        self.class_body_bound = BTreeSet::new();
        self.asm.emit(bc::Op::BuildClass);
        self.emit_store_name(name);
        Ok(())
    }

    /// Bind a class-body member's value (on the stack top) into the class namespace under its simple
    /// name (a NAMES-pool index), via `StoreName`.
    fn emit_store_member(&mut self, simple_name: &str) {
        let idx = self.name_index(simple_name);
        self.asm.emit(bc::Op::StoreName(idx));
    }

    /// The freevars of an already-compiled class method (hoisted under its qualified name) -- the
    /// enclosing-function locals it captures, loaded as cells at its `MakeFunction`. Empty for a
    /// top-level class (its methods compiled without an enclosing scope) or a method that captures
    /// nothing.
    fn method_freevars(&self, qualified: &str) -> Vec<String> {
        self.hoisted
            .iter()
            .find(|c| c.name == qualified)
            .map(|c| c.freevars.clone())
            .unwrap_or_default()
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
        let it_type = expr_static_type(iterable, &self.local_names, &self.local_types, &self.func_rets);
        if matches!(
            it_type,
            bc::StaticType::ListInt
                | bc::StaticType::ListFloat
                | bc::StaticType::TupleInt
                | bc::StaticType::TupleFloat
                | bc::StaticType::GrowListInt
                | bc::StaticType::GrowListFloat
        ) {
            return self.compile_for_iter_list(target, iterable, it_type, body, orelse);
        }
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
        self.emit_store_name(target);
        self.loops.push((top, after, self.finallys.len(), self.handler_depth));
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

    /// `for target in <typed list/tuple>: body` -- desugared to a counted loop `while i < len(it):
    /// target = it[i]; body; i += 1`, so the AOT lane runs it as array element loads (the same shape a
    /// `range(...)` loop uses). The iterable is evaluated ONCE into a hidden temp typed as the sequence
    /// (so the lowering recognizes its `len`/subscript as typed array ops); the counter temp is an int.
    /// The target holds the last element after the loop (Python's for-loop semantics), and `break`/
    /// `continue`/`else` behave exactly as in the range loop.
    fn compile_for_iter_list(
        &mut self,
        target: &str,
        iterable: &Expr,
        it_type: bc::StaticType,
        body: &[Stmt],
        orelse: &[Stmt],
    ) -> Result<(), CompileError> {
        let it_tmp = self.alloc_temp();
        self.local_types[it_tmp as usize] = it_type;
        self.compile_expr(iterable)?;
        self.asm.emit(bc::Op::StoreFast(it_tmp));
        let counter = self.alloc_temp();
        let zero = self.const_index(bc::Const::Int(0));
        self.asm.emit(bc::Op::LoadConst(zero));
        self.asm.emit(bc::Op::StoreFast(counter));
        let top = self.asm.new_label();
        let cont = self.asm.new_label();
        let else_label = self.asm.new_label();
        let after = self.asm.new_label();
        self.asm.place(top);
        self.asm.emit(bc::Op::LoadFast(counter));
        let len_idx = self.name_index("len");
        self.asm.emit(bc::Op::LoadGlobal(len_idx));
        self.asm.emit(bc::Op::LoadFast(it_tmp));
        self.asm.emit(bc::Op::Call(1));
        self.asm.emit(bc::Op::Compare(bc::CmpOp::Lt));
        self.asm.emit_branch(else_label);
        self.asm.emit(bc::Op::LoadFast(it_tmp));
        self.asm.emit(bc::Op::LoadFast(counter));
        let cache = self.asm.next_cache_slot();
        self.asm.emit(bc::Op::Subscript { cache });
        self.emit_store_name(target);
        self.loops.push((cont, after, self.finallys.len(), self.handler_depth));
        for stmt in body {
            self.compile_stmt(stmt)?;
        }
        self.loops.pop();
        self.asm.place(cont);
        self.asm.emit(bc::Op::LoadFast(counter));
        let one = self.const_index(bc::Const::Int(1));
        self.asm.emit(bc::Op::LoadConst(one));
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

    fn compile_expr(&mut self, expr: &Expr) -> Result<(), CompileError> {
        match expr {
            Expr::Int(value) => {
                let idx = self.const_index(bc::Const::Int(*value));
                self.asm.emit(bc::Op::LoadConst(idx));
            }
            Expr::Walrus { target, value } => {
                self.compile_expr(value)?;
                self.emit_store_name(target);
                self.emit_load_name(target);
            }
            Expr::Float(bits) => {
                let idx = self.const_index(bc::Const::Float(*bits));
                self.asm.emit(bc::Op::LoadConst(idx));
            }
            Expr::Imaginary(bits) => {
                let idx = self.const_index(bc::Const::Imaginary(*bits));
                self.asm.emit(bc::Op::LoadConst(idx));
            }
            Expr::BigInt(digits) => {
                let idx = self.const_index(bc::Const::BigInt(digits.clone()));
                self.asm.emit(bc::Op::LoadConst(idx));
            }
            Expr::Bytes(data) => {
                let idx = self.const_index(bc::Const::Bytes(data.clone()));
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
            Expr::Name(name) => self.emit_load_name(name),
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
                if comprehension_hoists(&[element], clauses) {
                    let body = build_container_comp_body(CompKind::List(element), clauses);
                    self.compile_hoisted_comprehension("listcomp", body, &clauses[0].iterable)?;
                } else {
                    self.compile_comprehension(CompKind::List(element), clauses)?;
                }
            }
            Expr::SetComp { element, clauses } => {
                if comprehension_hoists(&[element], clauses) {
                    let body = build_container_comp_body(CompKind::Set(element), clauses);
                    self.compile_hoisted_comprehension("setcomp", body, &clauses[0].iterable)?;
                } else {
                    self.compile_comprehension(CompKind::Set(element), clauses)?;
                }
            }
            Expr::DictComp {
                key,
                value,
                clauses,
            } => {
                if comprehension_hoists(&[key, value], clauses) {
                    let body = build_container_comp_body(CompKind::Dict(key, value), clauses);
                    self.compile_hoisted_comprehension("dictcomp", body, &clauses[0].iterable)?;
                } else {
                    self.compile_comprehension(CompKind::Dict(key, value), clauses)?;
                }
            }
            Expr::GeneratorExp { element, clauses } => {
                let body = build_genexpr_body(element, clauses);
                self.compile_hoisted_comprehension("genexpr", body, &clauses[0].iterable)?;
            }
            Expr::Binary { op, lhs, rhs } => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.asm.emit(bc::Op::Binary(binop_sel(*op)));
            }
            Expr::InplaceBinary { op, lhs, rhs } => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.asm.emit(bc::Op::InplaceBinOp(binop_sel(*op)));
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
            Expr::Call { func, args, keywords } => {
                if !keywords.is_empty() {
                    self.compile_expr(func)?;
                    for arg in args {
                        self.compile_expr(arg)?;
                    }
                    for kw in keywords {
                        self.compile_expr(&kw.value)?;
                    }
                    let names: Vec<String> = keywords.iter().map(|k| k.name.clone()).collect();
                    let kwnames = self.const_index(bc::Const::KwNames(names));
                    self.asm.emit(bc::Op::CallKw {
                        argc: args.len() as u32,
                        kwnames,
                    });
                } else if args.is_empty()
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
            Expr::CallEx { func, args } => {
                self.compile_expr(func)?;
                let mut kinds: Vec<u8> = Vec::with_capacity(args.len());
                let mut kwnames: Vec<String> = Vec::new();
                for arg in args {
                    match arg {
                        ast::CallArg::Positional(e) => {
                            self.compile_expr(e)?;
                            kinds.push(0);
                        }
                        ast::CallArg::Star(e) => {
                            self.compile_expr(e)?;
                            kinds.push(1);
                        }
                        ast::CallArg::Keyword(name, e) => {
                            self.compile_expr(e)?;
                            kinds.push(2);
                            kwnames.push(name.clone());
                        }
                        ast::CallArg::DoubleStar(e) => {
                            self.compile_expr(e)?;
                            kinds.push(3);
                        }
                    }
                }
                let kinds_idx = self.const_index(bc::Const::ArgKinds(kinds));
                let kwnames_idx = self.const_index(bc::Const::KwNames(kwnames));
                self.asm.emit(bc::Op::CallEx {
                    argc: args.len() as u32,
                    kinds: kinds_idx,
                    kwnames: kwnames_idx,
                });
            }
            Expr::Lambda { params, body } => self.compile_lambda(params, body)?,
            Expr::Yield(value) => {
                match value {
                    Some(v) => self.compile_expr(v)?,
                    None => {
                        let none = self.const_index(bc::Const::None);
                        self.asm.emit(bc::Op::LoadConst(none));
                    }
                }
                self.asm.emit(bc::Op::Yield);
                self.has_yield = true;
            }
            Expr::YieldFrom(value) => {
                self.compile_expr(value)?;
                self.asm.emit(bc::Op::GetIter);
                self.asm.emit(bc::Op::YieldFrom);
                self.has_yield = true;
            }
        }
        Ok(())
    }

    /// Whether the direct module-level def now being compiled is the LAST of its name, and so the one
    /// whose body the function table holds under that bare name. Counts each def as it is compiled, so
    /// it must be called exactly once per direct def, in source order -- the same walk, over the same
    /// statements, that `compile_module` used to decide which def to put in the table.
    fn owns_module_def_name(&mut self, name: &str) -> bool {
        let seen = self.module_defs_seen.entry(String::from(name)).or_insert(0);
        *seen += 1;
        self.module_def_totals.get(name) == Some(seen)
    }

    /// Emit a module-level def that OWNS its name at its source position: any default values, then
    /// `MakeFunction` resolving the function by that bare name (the table entry `compile_module`
    /// hoisted), then `StoreName` -- which set_global's the PyFunction, so a later `LoadGlobal` prefers
    /// it (it carries the defaults) over the plain function-table ref.
    ///
    /// A def binds its name where it STANDS, which is why this is emitted at the source position and
    /// not just left to the table: `g = 1; def g(): ...` must leave `g` the function, and a default may
    /// reference an earlier module var.
    fn compile_module_def(&mut self, func: &FuncDef) -> Result<(), CompileError> {
        let flags = self.emit_param_defaults(&func.params)?;
        let func_name = self.name_index(&func.name);
        self.asm.emit(bc::Op::MakeFunction {
            func: func_name,
            flags,
        });
        self.emit_store_name(&func.name);
        Ok(())
    }

    /// Compile a module-level `def` that does NOT own its name AT ITS DEF SITE: hoist its `CodeObject`
    /// into the module function table under a UNIQUE synthetic name, then emit the def-site
    /// `MakeFunction` (referencing that unique name) + `StoreName` of the def's real name. The unique
    /// name is what makes each such def resolve to ITS OWN body rather than to whatever the table holds
    /// under the bare one.
    ///
    /// Two kinds of def land here. One buried in a block (`if`/`for`/`while`/`try`/`with`) -- bound only
    /// when its block runs, and where sibling same-named defs (a version guard `if c: def f else: def f`)
    /// must stay DISTINCT code objects. And one a LATER def rebinds: it owns the name only until that
    /// later def stands, while the table's bare name belongs to the last of them.
    ///
    /// Module scope has no cells, so either captures nothing (every enclosing name is a global) --
    /// compiled against an empty scope chain, exactly like a def that does own its name.
    fn compile_hoisted_module_def(&mut self, func: &FuncDef) -> Result<(), CompileError> {
        let seq = self.block_def_counter;
        self.block_def_counter += 1;
        let qualified = format!("{}.${seq}.{}", self.name, func.name);
        let body: Vec<&Stmt> = func.body.iter().collect();
        let (co, hoisted) = compile_code_object(
            Scope::Function,
            &qualified,
            &func.params,
            &func.ret,
            &body,
            None,
            Outer { enclosing: &[], func_rets: &self.func_rets },
        )?;
        self.hoisted.push(co);
        self.hoisted.extend(hoisted);
        let flags = self.emit_param_defaults(&func.params)?;
        let idx = self.name_index(&qualified);
        self.asm.emit(bc::Op::MakeFunction { func: idx, flags });
        self.emit_store_name(&func.name);
        Ok(())
    }

    /// Emit a def / lambda / method's default operands at its def site, bottom-to-top to match
    /// MakeFunction's pop order: the positional-defaults TUPLE (flag bit0 = 0x01), then the
    /// keyword-only-defaults DICT `{name: value}` (flag bit1 = 0x02). Returns the combined flag bits
    /// (the caller ORs in bit2 = 0x04 for closure cells, which it pushes on top). The default
    /// expressions are evaluated here, in the enclosing scope.
    fn emit_param_defaults(&mut self, params: &[ast::ParamDef]) -> Result<u8, CompileError> {
        let mut flags = 0u8;
        let n_pos = params
            .iter()
            .filter(|p| !p.keyword_only && p.default.is_some())
            .count() as u32;
        if n_pos > 0 {
            for p in params {
                if !p.keyword_only {
                    if let Some(default) = &p.default {
                        self.compile_expr(default)?;
                    }
                }
            }
            self.asm.emit(bc::Op::BuildTuple(n_pos));
            flags |= 0x01;
        }
        let n_kw = params
            .iter()
            .filter(|p| p.keyword_only && p.default.is_some())
            .count() as u32;
        if n_kw > 0 {
            for p in params {
                if p.keyword_only {
                    if let Some(default) = &p.default {
                        let key = self.const_index(bc::Const::Str(p.name.clone()));
                        self.asm.emit(bc::Op::LoadConst(key));
                        self.compile_expr(default)?;
                    }
                }
            }
            self.asm.emit(bc::Op::BuildDict(n_kw));
            flags |= 0x02;
        }
        Ok(flags)
    }

    /// Compile a nested `def` as a hoisted closure. Its body becomes a `CodeObject` named for its
    /// nesting path (so the module function table stays flat), carrying its own cellvars/freevars
    /// analyzed against this function's scope chain. At the def site emit any positional-defaults
    /// tuple, then one `LoadClosure` per captured free variable, then `MakeFunction` with the
    /// CLOSURE (and defaults) flags, then bind the def's name in this scope. Two sibling defs sharing
    /// a name -- `if c: def f ... else: def f ...`, capturing DIFFERENT enclosing vars -- get DISTINCT
    /// code-object names (`scope.f`, then `scope.f$1`), so each site's `MakeFunction` resolves to its
    /// OWN body + freevar layout (else the second bound the first's closure).
    fn compile_nested_def(&mut self, func: &FuncDef) -> Result<(), CompileError> {
        let seq = {
            let count = self.nested_def_counts.entry(func.name.clone()).or_insert(0);
            let s = *count;
            *count += 1;
            s
        };
        let qualified = if seq == 0 {
            format!("{}.{}", self.name, func.name)
        } else {
            format!("{}.{}${seq}", self.name, func.name)
        };
        let body: Vec<&Stmt> = func.body.iter().collect();
        let (co, hoisted) = compile_code_object(
            Scope::Function,
            &qualified,
            &func.params,
            &func.ret,
            &body,
            None,
            Outer { enclosing: &self.child_scopes, func_rets: &self.func_rets },
        )?;
        let freevars = co.freevars.clone();
        self.hoisted.push(co);
        self.hoisted.extend(hoisted);
        let mut flags = self.emit_param_defaults(&func.params)?;
        flags |= self.emit_captured_cells(&freevars);
        let func_idx = self.name_index(&qualified);
        self.asm.emit(bc::Op::MakeFunction { func: func_idx, flags });
        self.emit_store_name(&func.name);
        Ok(())
    }

    /// Compile a lambda: hoist it to a synthetic function (named for uniqueness) and reference it
    /// with `MakeFunction`. Its body is `return <body>`. A lambda that captures an enclosing
    /// function's local is a closure -- its free variables are pushed as cells (`LoadClosure`) and
    /// `MakeFunction` carries the CLOSURE flag, exactly like a nested def. At module scope there is
    /// no enclosing function, so a lambda there captures nothing (every name is a global).
    fn compile_lambda(
        &mut self,
        params: &[ast::ParamDef],
        body: &Expr,
    ) -> Result<(), CompileError> {
        let lambda_name = format!("{}.<lambda.{}>", self.name, self.lambda_counter);
        self.lambda_counter += 1;
        let lambda_body = [Stmt::Return(Some(body.clone()))];
        let body_refs: Vec<&Stmt> = lambda_body.iter().collect();
        let (lambda_co, nested) = compile_code_object(
            Scope::Function,
            &lambda_name,
            params,
            &None,
            &body_refs,
            None,
            Outer { enclosing: &self.child_scopes, func_rets: &self.func_rets },
        )?;
        let freevars = lambda_co.freevars.clone();
        self.hoisted.push(lambda_co);
        self.hoisted.extend(nested);
        let mut flags = self.emit_param_defaults(params)?;
        flags |= self.emit_captured_cells(&freevars);
        let idx = self.name_index(&lambda_name);
        self.asm.emit(bc::Op::MakeFunction { func: idx, flags });
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

    /// Compile a comprehension as its own function scope: hoist `body` -- which builds and returns
    /// the container, or (for a genexpr) yields each element -- into a hidden `<tag.N>(.0)` function,
    /// then at the call site build the (closure) function and call it with the eagerly-iter'd
    /// outermost iterable. The loop targets are the hidden function's locals, so they never leak into
    /// this scope. Shared by list/set/dict comprehensions and generator expressions.
    fn compile_hoisted_comprehension(
        &mut self,
        tag: &str,
        body: Vec<Stmt>,
        first_iterable: &Expr,
    ) -> Result<(), CompileError> {
        let name = format!("{}.<{}.{}>", self.name, tag, self.lambda_counter);
        self.lambda_counter += 1;
        let params = [genexpr_param()];
        let body_refs: Vec<&Stmt> = body.iter().collect();
        let (co, nested) = compile_code_object(
            Scope::Function,
            &name,
            &params,
            &None,
            &body_refs,
            None,
            Outer { enclosing: &self.child_scopes, func_rets: &self.func_rets },
        )?;
        let freevars = co.freevars.clone();
        self.hoisted.push(co);
        self.hoisted.extend(nested);
        let flags = self.emit_captured_cells(&freevars);
        let idx = self.name_index(&name);
        self.asm.emit(bc::Op::MakeFunction { func: idx, flags });
        self.compile_expr(first_iterable)?;
        self.asm.emit(bc::Op::GetIter);
        self.asm.emit(bc::Op::Call(1));
        Ok(())
    }

    /// Compile a comprehension INLINE (the fallback when it holds a walrus that must leak): build an
    /// empty container in a temp, run the clause chain (nested loops, each with its `if` filters),
    /// append/insert the element at the innermost point, and leave the container on the stack.
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
            self.emit_store_name(t);
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
        let f = self.const_index(bc::Const::Bool(false));
        self.asm.emit(bc::Op::LoadConst(f));
        self.asm.emit(bc::Op::StoreFast(tmp));
        self.asm.emit_jump(end);
        self.asm.place(falsey);
        let t = self.const_index(bc::Const::Bool(true));
        self.asm.emit(bc::Op::LoadConst(t));
        self.asm.emit(bc::Op::StoreFast(tmp));
        self.asm.place(end);
        self.asm.emit(bc::Op::LoadFast(tmp));
        Ok(())
    }

    /// Store the value on top of the stack (an unpacked element, or the starred list) into one
    /// tuple-unpacking target. The element sits BELOW any container/index/obj we compile, which is
    /// exactly the stack order Setitem `[value, container, index]` / SetAttr `[value, obj]` expect.
    fn compile_unpack_target(&mut self, target: &ast::AssignTarget) -> Result<(), CompileError> {
        match target {
            ast::AssignTarget::Name(name) => self.emit_store_name(name),
            ast::AssignTarget::Subscript { container, index } => {
                self.compile_expr(container)?;
                self.compile_expr(index)?;
                self.asm.emit(bc::Op::Setitem);
            }
            ast::AssignTarget::Attribute { obj, attr } => {
                self.compile_expr(obj)?;
                let name = self.name_index(attr);
                let cache = self.asm.next_cache_slot();
                self.asm.emit(bc::Op::SetAttr { name, cache });
            }
            ast::AssignTarget::Tuple(targets) => {
                self.asm.emit(bc::Op::UnpackSequence(targets.len() as u32));
                for t in targets {
                    self.compile_unpack_target(t)?;
                }
            }
        }
        Ok(())
    }

    /// Compile `c[i] = v` (op None) or `c[i] OP= v` (op Some). The augmented form evaluates the
    /// container and index ONCE into temps, then `_c[_i] = _c[_i] OP v` -- so a side-effecting
    /// container/index runs exactly once (Python semantics), unlike a `c[i] = c[i] OP v` desugar.
    fn compile_setitem(
        &mut self,
        container: &Expr,
        index: &Expr,
        value: &Expr,
        op: Option<ast::BinOp>,
    ) -> Result<(), CompileError> {
        let Some(op) = op else {
            self.compile_expr(value)?;
            self.compile_expr(container)?;
            self.compile_expr(index)?;
            self.asm.emit(bc::Op::Setitem);
            return Ok(());
        };
        let c = self.alloc_temp();
        let i = self.alloc_temp();
        self.compile_expr(container)?;
        self.asm.emit(bc::Op::StoreFast(c));
        self.compile_expr(index)?;
        self.asm.emit(bc::Op::StoreFast(i));
        let cn = Expr::Name(format!(".t{c}"));
        let ix = Expr::Name(format!(".t{i}"));
        let combined = Expr::InplaceBinary {
            op,
            lhs: Box::new(Expr::Subscript {
                value: Box::new(cn.clone()),
                index: Box::new(ix.clone()),
            }),
            rhs: Box::new(value.clone()),
        };
        self.compile_setitem(&cn, &ix, &combined, None)
    }

    /// Compile `obj.attr = v` (op None) or `obj.attr OP= v` (op Some). The augmented form evaluates
    /// `obj` once into a temp, then `_o.attr = _o.attr OP v`.
    fn compile_setattr(
        &mut self,
        obj: &Expr,
        attr: &str,
        value: &Expr,
        op: Option<ast::BinOp>,
    ) -> Result<(), CompileError> {
        let Some(op) = op else {
            self.compile_expr(value)?;
            self.compile_expr(obj)?;
            let name = self.name_index(attr);
            let cache = self.asm.next_cache_slot();
            self.asm.emit(bc::Op::SetAttr { name, cache });
            return Ok(());
        };
        let o = self.alloc_temp();
        self.compile_expr(obj)?;
        self.asm.emit(bc::Op::StoreFast(o));
        let on = Expr::Name(format!(".t{o}"));
        let combined = Expr::InplaceBinary {
            op,
            lhs: Box::new(Expr::Attribute {
                value: Box::new(on.clone()),
                attr: String::from(attr),
            }),
            rhs: Box::new(value.clone()),
        };
        self.compile_setattr(&on, attr, &combined, None)
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
        ast::BinOp::TrueDiv => bc::BinOp::TrueDiv,
        ast::BinOp::Pow => bc::BinOp::Pow,
        ast::BinOp::MatMul => bc::BinOp::MatMul,
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
        ast::CmpOp::Is => bc::CmpOp::Is,
        ast::CmpOp::IsNot => bc::CmpOp::IsNot,
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
    fn keyword_arguments_emit_callkw() {
        let module = compile_src("def g(a):\n    return a\ng(x=1)\n").unwrap();
        let (argc, kwnames) = module
            .body
            .ops
            .iter()
            .find_map(|op| match op {
                Op::CallKw { argc, kwnames } => Some((*argc, *kwnames)),
                _ => None,
            })
            .expect("a CallKw op");
        assert_eq!(argc, 0);
        assert_eq!(
            module.body.consts.get(kwnames as usize),
            Some(&Const::KwNames(vec![String::from("x")]))
        );
    }

    #[test]
    fn a_mixed_positional_and_keyword_call_orders_the_names() {
        let module = compile_src("def h(a):\n    return a\nh(1, 2, y=3, z=4)\n").unwrap();
        let (argc, kwnames) = module
            .body
            .ops
            .iter()
            .find_map(|op| match op {
                Op::CallKw { argc, kwnames } => Some((*argc, *kwnames)),
                _ => None,
            })
            .expect("a CallKw op");
        assert_eq!(argc, 2);
        assert_eq!(
            module.body.consts.get(kwnames as usize),
            Some(&Const::KwNames(vec![String::from("y"), String::from("z")]))
        );
    }

    #[test]
    fn a_module_level_defaulted_def_emits_its_def_site() {
        let module = compile_src("def f(a, b=1):\n    return a + b\nprint(f(5))\n").unwrap();
        assert!(module.body.ops.iter().any(|op| matches!(op, Op::BuildTuple(1))));
        assert!(module
            .body
            .ops
            .iter()
            .any(|op| matches!(op, Op::MakeFunction { flags: 1, .. })));
        assert!(module.functions.iter().any(|co| co.name == "f"));
    }

    #[test]
    fn a_plain_def_binds_its_name_at_its_def_site() {
        let module = compile_src("def f(a):\n    return a\nprint(f(5))\n").unwrap();
        assert_eq!(
            module.body.ops.iter().filter(|op| matches!(op, Op::MakeFunction { .. })).count(),
            1
        );
        assert!(module.functions.iter().any(|c| c.name == "f"));
    }

    #[test]
    fn a_module_def_nested_in_a_block_is_hoisted_into_the_function_table() {
        let module = compile_src("if True:\n    def f():\n        return 1\nprint(f())\n").unwrap();
        assert!(
            module.functions.iter().any(|c| c.name.contains('$') && c.name.ends_with(".f")),
            "the block-nested def is hoisted under a synthetic name"
        );
        assert!(module.body.ops.iter().any(|op| matches!(op, Op::MakeFunction { .. })));
    }

    #[test]
    fn sibling_same_named_module_defs_get_distinct_code_objects() {
        let module = compile_src(
            "if c:\n    def f():\n        return 1\nelse:\n    def f():\n        return 2\n",
        )
        .unwrap();
        let fs: Vec<&str> = module
            .functions
            .iter()
            .map(|c| c.name.as_str())
            .filter(|n| n.ends_with(".f"))
            .collect();
        assert_eq!(fs.len(), 2, "two distinct code objects for the sibling defs");
        assert_ne!(fs[0], fs[1], "distinct synthetic names");
    }

    #[test]
    fn a_module_class_nested_in_a_block_hoists_its_methods() {
        let module = compile_src(
            "try:\n    class C:\n        def m(self):\n            return 1\nexcept Exception:\n    pass\n",
        )
        .unwrap();
        assert!(
            module.functions.iter().any(|c| c.name.contains('$') && c.name.ends_with(".m")),
            "the block-nested class's method is hoisted"
        );
    }

    #[test]
    fn the_last_module_def_of_a_name_owns_the_function_table_entry() {
        let module = compile_src(
            "def f():\n    return 1\na = f\ndef f():\n    return 2\nprint(a(), f())\n",
        )
        .unwrap();
        let bare: Vec<&bc::CodeObject> = module.functions.iter().filter(|c| c.name == "f").collect();
        assert_eq!(bare.len(), 1, "exactly one table entry owns the bare name");
        assert!(bare[0].ops.iter().any(|op| matches!(op, Op::LoadConst(i) if bare[0].consts[*i as usize] == bc::Const::Int(2))));
        assert_eq!(
            module.functions.iter().filter(|c| c.name.contains('$') && c.name.ends_with(".f")).count(),
            1
        );
    }

    #[test]
    fn a_def_rebinds_a_name_an_earlier_assignment_bound() {
        let module = compile_src("g = 1\ndef g():\n    return 2\nprint(g())\n").unwrap();
        let ops = &module.body.ops;
        let store_of_int = ops.iter().position(|op| matches!(op, Op::StoreName(_) | Op::StoreGlobal(_)));
        let make = ops.iter().position(|op| matches!(op, Op::MakeFunction { .. }));
        assert!(make.is_some(), "the def emits at its site");
        assert!(store_of_int < make, "the def's binding comes AFTER the `g = 1` store");
    }

    #[test]
    fn a_decorated_def_does_not_own_its_bare_name() {
        let module = compile_src("def d(fn):\n    return fn\n@d\ndef f():\n    return 1\nprint(f())\n").unwrap();
        assert!(
            !module.functions.iter().any(|c| c.name == "f"),
            "no bare table entry for a decorated def"
        );
        assert_eq!(
            module.functions.iter().filter(|c| c.name.contains('$') && c.name.ends_with(".f")).count(),
            1,
            "its body is hoisted under a synthetic name"
        );
    }

    #[test]
    fn a_decorated_def_counts_toward_its_name_s_is_last_check() {
        let module = compile_src(
            "def d(fn):\n    return fn\n@d\ndef f():\n    return 1\ndef f():\n    return 2\nprint(f())\n",
        )
        .unwrap();
        let bare: Vec<&bc::CodeObject> = module.functions.iter().filter(|c| c.name == "f").collect();
        assert_eq!(bare.len(), 1, "the plain last def owns the bare name");
        assert!(bare[0].ops.iter().any(|op| matches!(op, Op::LoadConst(i) if bare[0].consts[*i as usize] == bc::Const::Int(2))));
    }

    #[test]
    fn sibling_same_named_nested_defs_get_distinct_code_objects() {
        let module = compile_src(
            "def make(a, b):\n    if a:\n        def f():\n            return a\n    else:\n        def f():\n            return b\n    return f\n",
        )
        .unwrap();
        let fs: Vec<&str> = module
            .functions
            .iter()
            .map(|c| c.name.as_str())
            .filter(|n| n.starts_with("make.f"))
            .collect();
        assert_eq!(fs.len(), 2, "two distinct code objects for the sibling defs");
        assert!(fs.contains(&"make.f"), "the first keeps the bare qualified name");
        assert!(fs.iter().any(|n| n.contains('$')), "the sibling is suffixed");
    }

    #[test]
    fn a_single_nested_def_keeps_its_bare_qualified_name() {
        let module = compile_src(
            "def make_adder(n):\n    def add(x):\n        return x + n\n    return add\n",
        )
        .unwrap();
        assert!(module.functions.iter().any(|c| c.name == "make_adder.add"));
        assert!(!module.functions.iter().any(|c| c.name.contains('$')));
    }

    #[test]
    fn keyword_only_params_set_kwonly_count() {
        let module = compile_src("def f(a, b, *, c, d):\n    return a\n").unwrap();
        let f = func(&module, "f");
        assert_eq!(f.params.len(), 4);
        assert_eq!(f.kwonly_count, 2);
        assert_eq!(f.posonly_count, 0);
    }

    #[test]
    fn varargs_sets_has_varargs() {
        let module = compile_src("def f(a, *args):\n    return a\n").unwrap();
        let f = func(&module, "f");
        assert!(f.has_varargs);
        assert!(!f.has_varkwargs);
        assert_eq!(f.params.len(), 2);
    }

    #[test]
    fn varkwargs_sets_has_varkwargs() {
        let module = compile_src("def f(a, **kw):\n    return a\n").unwrap();
        let f = func(&module, "f");
        assert!(f.has_varkwargs);
        assert!(!f.has_varargs);
        assert_eq!(f.params.len(), 2);
    }

    #[test]
    fn float_literal_emits_a_float_const() {
        let module = compile_src("x = 3.14\nprint(x)\n").unwrap();
        assert!(module
            .body
            .consts
            .iter()
            .any(|c| matches!(c, bc::Const::Float(bits) if f64::from_bits(*bits) == 3.14)));
    }

    #[test]
    fn del_of_a_subscript_or_attribute_compiles_to_delete_ops() {
        let item = compile_src("xs = [1]\ndel xs[0]\n").unwrap();
        assert!(item.body.ops.iter().any(|op| matches!(op, Op::DeleteItem)));
        let attr = compile_src("o = make()\ndel o.attr\n").unwrap();
        assert!(attr.body.ops.iter().any(|op| matches!(op, Op::DeleteAttr { .. })));
    }

    #[test]
    fn with_desugars_to_the_enter_exit_protocol() {
        let module = compile_src("with mgr() as x:\n    print(x)\n").unwrap();
        assert!(module.body.names.iter().any(|n| n == "__enter__"));
        assert!(module.body.names.iter().any(|n| n == "__exit__"));
        assert!(module.body.names.iter().any(|n| n == "type"), "type(exc) passes the exception type");
        assert!(module.body.ops.iter().any(|op| matches!(op, Op::PopExcept)), "the exception handler");
        assert!(module.body.ops.iter().any(|op| matches!(op, Op::Reraise)));
    }

    #[test]
    fn a_generator_function_emits_yield_and_is_flagged() {
        let module = compile_src("def g():\n    yield 1\n    yield 2\n").unwrap();
        let g = func(&module, "g");
        assert!(g.is_generator);
        assert_eq!(g.ops.iter().filter(|op| matches!(op, Op::Yield)).count(), 2);
        let plain = compile_src("def h():\n    return 5\n").unwrap();
        assert!(!func(&plain, "h").is_generator);
    }

    #[test]
    fn yield_from_emits_getiter_then_yieldfrom_and_flags_generator() {
        let module = compile_src("def g():\n    yield from xs\n").unwrap();
        let g = func(&module, "g");
        assert!(g.is_generator, "yield from makes the function a generator");
        let gi = g.ops.iter().position(|op| matches!(op, Op::GetIter));
        let yf = g.ops.iter().position(|op| matches!(op, Op::YieldFrom));
        assert!(gi.is_some() && yf.is_some(), "emits GetIter and YieldFrom");
        assert!(gi < yf, "GetIter (obtain the sub-iterator) precedes YieldFrom (delegate)");
    }

    #[test]
    fn genexpr_compiles_to_a_lazy_generator_function() {
        let module = compile_src("g = (x * x for x in xs)\n").unwrap();
        let genfn = module
            .functions
            .iter()
            .find(|f| f.name.contains("<genexpr."))
            .expect("a hoisted genexpr function");
        assert!(genfn.is_generator, "the genexpr function is a generator");
        assert!(genfn.ops.iter().any(|op| matches!(op, Op::Yield)));
        assert_eq!(genfn.params.len(), 1, "the .0 parameter (the outermost iterable)");
        assert!(module.body.ops.iter().any(|op| matches!(op, Op::MakeFunction { .. })));
        assert!(module.body.ops.iter().any(|op| matches!(op, Op::GetIter)));
        assert!(module.body.ops.iter().any(|op| matches!(op, Op::Call(1))));
        assert!(!module.body.ops.iter().any(|op| matches!(op, Op::ListAppend)));
    }

    #[test]
    fn lambda_hoists_to_a_function_and_emits_makefunction() {
        let module = compile_src("f = lambda x: x + 1\n").unwrap();
        let lam = module
            .functions
            .iter()
            .find(|f| f.name.contains("<lambda."))
            .expect("hoisted lambda function present");
        assert_eq!(lam.params.len(), 1);
        assert_eq!(lam.params[0].name, "x");
        assert!(matches!(lam.ops.last(), Some(Op::Return)));
        assert!(
            module.body.ops.iter().any(|op| matches!(op, Op::MakeFunction { .. })),
            "the module body references the lambda with MakeFunction"
        );
    }

    #[test]
    fn lambda_with_a_default_emits_the_defaults_tuple() {
        let module = compile_src("f = lambda a, b=10: a + b\n").unwrap();
        assert!(
            module.body.ops.iter().any(|op| matches!(op, Op::BuildTuple(1))),
            "the (10,) defaults tuple"
        );
        assert!(
            module.body.ops.iter().any(|op| matches!(op, Op::MakeFunction { flags, .. } if flags & 0x01 != 0)),
            "MakeFunction carries the defaults flag"
        );
        let plain = compile_src("g = lambda a: a\n").unwrap();
        assert!(plain
            .body
            .ops
            .iter()
            .any(|op| matches!(op, Op::MakeFunction { flags, .. } if flags & 0x01 == 0)));
        let method = compile_src("class C:\n    def m(self, x=1):\n        return x\n").unwrap();
        assert!(method.functions.iter().any(|c| c.name == "C.m"), "the method body compiled");
    }

    #[test]
    fn lambda_capturing_an_enclosing_local_is_a_closure() {
        let module =
            compile_src("def f():\n    n = 5\n    g = lambda: n\n    return g\n").unwrap();
        let f = module.functions.iter().find(|c| c.name == "f").expect("f present");
        assert_eq!(f.cellvars.len(), 1);
        assert_eq!(f.cellvars[0], "n");
        assert!(f.ops.iter().any(|op| matches!(op, Op::StoreDeref(0))));
        assert!(f.ops.iter().any(|op| matches!(op, Op::LoadClosure(0))));
        assert!(f
            .ops
            .iter()
            .any(|op| matches!(op, Op::MakeFunction { flags, .. } if flags & 0x04 != 0)));
        let lam = module
            .functions
            .iter()
            .find(|c| c.name.contains("<lambda."))
            .expect("lambda present");
        assert_eq!(lam.freevars.len(), 1);
        assert_eq!(lam.freevars[0], "n");
        assert!(lam.ops.iter().any(|op| matches!(op, Op::LoadDeref(0))));
    }

    #[test]
    fn a_nested_def_capturing_a_param_is_a_closure() {
        let src = "def make_adder(k):\n    def add(n):\n        return n + k\n    return add\n";
        let module = compile_src(src).unwrap();
        let outer = func(&module, "make_adder");
        assert_eq!(outer.cellvars, [String::from("k")]);
        assert!(outer.freevars.is_empty());
        assert!(outer.ops.iter().any(|op| matches!(op, Op::LoadClosure(0))));
        assert!(outer
            .ops
            .iter()
            .any(|op| matches!(op, Op::MakeFunction { flags, .. } if flags & 0x04 != 0)));
        let inner = func(&module, "make_adder.add");
        assert!(inner.cellvars.is_empty());
        assert_eq!(inner.freevars, [String::from("k")]);
        assert_eq!(
            &inner.ops[..4],
            &[Op::LoadFast(0), Op::LoadDeref(0), Op::Binary(BinOp::Add), Op::Return]
        );
    }

    #[test]
    fn a_nested_class_method_capturing_a_param_is_a_closure() {
        let src = "def make_adder(n):\n    class Adder:\n        def add(self, x):\n            \
                   return x + n\n    return Adder()\n";
        let module = compile_src(src).unwrap();
        let outer = func(&module, "make_adder");
        assert_eq!(outer.cellvars, [String::from("n")]);
        assert!(outer.freevars.is_empty());
        assert!(outer.ops.iter().any(|op| matches!(op, Op::LoadClosure(0))));
        assert!(outer
            .ops
            .iter()
            .any(|op| matches!(op, Op::MakeFunction { flags, .. } if flags & 0x04 != 0)));
        let method = func(&module, "make_adder.Adder.add");
        assert!(method.cellvars.is_empty());
        assert_eq!(method.freevars, [String::from("n")]);
        assert_eq!(
            &method.ops[..4],
            &[Op::LoadFast(1), Op::LoadDeref(0), Op::Binary(BinOp::Add), Op::Return]
        );
    }

    #[test]
    fn a_top_level_class_method_reads_a_global_not_a_cell() {
        let module = compile_src("G = 10\nclass C:\n    def m(self):\n        return G\n").unwrap();
        let m = func(&module, "C.m");
        assert!(m.cellvars.is_empty());
        assert!(m.freevars.is_empty());
        assert!(m.ops.iter().any(|op| matches!(op, Op::LoadGlobal(_))));
        assert!(!m.ops.iter().any(|op| matches!(op, Op::LoadDeref(_))));
    }

    #[test]
    fn a_nested_class_body_reads_an_enclosing_local_directly() {
        let m = compile_src("def f(n):\n    class A:\n        x = n\n    return A\n").unwrap();
        let f = func(&m, "f");
        assert!(f.cellvars.is_empty());
        assert!(f.ops.iter().any(|op| matches!(op, Op::LoadFast(0))));
    }

    #[test]
    fn a_class_attribute_shadowing_an_outer_name_resolves_namespace_first() {
        let m = compile_src("G = 99\nclass A:\n    G = 5\n    x = G\n").unwrap();
        let g_idx = m.body.names.iter().position(|n| n == "G").map(|i| i as u32);
        assert!(
            m.body.ops.iter().any(|op| matches!(op, Op::LoadName(i) if Some(*i) == g_idx)),
            "the shadowing class attribute reads namespace-first (LoadName), not LoadFast"
        );
    }

    #[test]
    fn a_freevar_bubbles_through_an_intermediate_closure() {
        let src = "def repeat(times):\n    def deco(fn):\n        def wrapper(x):\n            \
                   return fn(x) + times\n        return wrapper\n    return deco\n";
        let module = compile_src(src).unwrap();

        let repeat = func(&module, "repeat");
        assert_eq!(repeat.cellvars, [String::from("times")]);
        assert!(repeat.freevars.is_empty());

        let deco = func(&module, "repeat.deco");
        assert_eq!(deco.cellvars, [String::from("fn")]);
        assert_eq!(deco.freevars, [String::from("times")]);
        let closures: Vec<u32> = deco
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::LoadClosure(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(closures, [0, 1]);

        let wrapper = func(&module, "repeat.deco.wrapper");
        assert!(wrapper.cellvars.is_empty());
        assert_eq!(wrapper.freevars, [String::from("fn"), String::from("times")]);
        assert!(wrapper.ops.iter().any(|op| matches!(op, Op::LoadDeref(0))));
        assert!(wrapper.ops.iter().any(|op| matches!(op, Op::LoadDeref(1))));
    }

    #[test]
    fn a_top_level_function_reads_a_global_not_a_cell() {
        let module = compile_src("G = 10\ndef f():\n    return G\n").unwrap();
        let f = func(&module, "f");
        assert!(f.cellvars.is_empty());
        assert!(f.freevars.is_empty());
        assert!(f.ops.iter().any(|op| matches!(op, Op::LoadGlobal(_))));
        assert!(!f.ops.iter().any(|op| matches!(op, Op::LoadDeref(_))));
    }

    #[test]
    fn a_closure_mutating_a_captured_list_reads_it_through_a_cell() {
        let src = "def make_counter():\n    box = [0]\n    def step():\n        \
                   box[0] = box[0] + 1\n        return box[0]\n    return step\n";
        let module = compile_src(src).unwrap();
        assert_eq!(func(&module, "make_counter").cellvars, [String::from("box")]);
        let step = func(&module, "make_counter.step");
        assert_eq!(step.freevars, [String::from("box")]);
        assert!(step.ops.iter().any(|op| matches!(op, Op::LoadDeref(0))));
        assert!(step.ops.iter().any(|op| matches!(op, Op::Setitem)));
        assert!(!step.ops.iter().any(|op| matches!(op, Op::StoreDeref(_))));
    }

    #[test]
    fn nonlocal_makes_a_write_through_cell() {
        let src = "def make_counter():\n    n = 0\n    def step():\n        nonlocal n\n        \
                   n = n + 1\n        return n\n    return step\n";
        let module = compile_src(src).unwrap();
        let outer = func(&module, "make_counter");
        assert_eq!(outer.cellvars, [String::from("n")]);
        assert!(outer.ops.iter().any(|op| matches!(op, Op::StoreDeref(0))), "n = 0 writes the cell");
        let step = func(&module, "make_counter.step");
        assert!(step.cellvars.is_empty());
        assert_eq!(step.freevars, [String::from("n")]);
        assert!(step.ops.iter().any(|op| matches!(op, Op::LoadDeref(0))));
        assert!(step.ops.iter().any(|op| matches!(op, Op::StoreDeref(0))), "n is written through");
        assert!(!step.local_names.iter().any(|n| n == "n"));
    }

    #[test]
    fn nonlocal_without_an_enclosing_binding_is_rejected() {
        let top = compile_src("def f():\n    nonlocal x\n    x = 1\n").unwrap_err();
        assert!(top.message.contains("no binding for nonlocal 'x'"), "{}", top.message);
        let deeper = compile_src(
            "def outer():\n    def inner():\n        nonlocal y\n        y = 1\n    return inner\n",
        )
        .unwrap_err();
        assert!(deeper.message.contains("no binding for nonlocal 'y'"), "{}", deeper.message);
    }

    #[test]
    fn nonlocal_on_a_parameter_is_rejected() {
        let err = compile_src(
            "def outer():\n    a = 0\n    def inner(a):\n        nonlocal a\n        return a\n    return inner\n",
        )
        .unwrap_err();
        assert!(err.message.contains("parameter and nonlocal"), "{}", err.message);
    }

    #[test]
    fn global_reads_and_writes_the_module_namespace() {
        let src = "def bump():\n    global count\n    count = count + 1\n    return count\n";
        let module = compile_src(src).unwrap();
        let f = func(&module, "bump");
        assert!(!f.local_names.iter().any(|n| n == "count"), "count is not a local slot");
        assert!(f.cellvars.is_empty() && f.freevars.is_empty(), "a global is not a cell/free var");
        assert!(
            f.ops.iter().any(|op| matches!(op, Op::LoadGlobal(_))),
            "count is read through the global namespace"
        );
        assert!(
            f.ops.iter().any(|op| matches!(op, Op::StoreGlobal(_))),
            "count is written through the global namespace"
        );
    }

    #[test]
    fn global_on_a_parameter_is_rejected() {
        let err = compile_src("def f(a):\n    global a\n    return a\n").unwrap_err();
        assert!(err.message.contains("parameter and global"), "{}", err.message);
    }

    #[test]
    fn a_decorated_method_lands_in_the_class_namespace() {
        let src = "def tag(f):\n    return f\nclass C:\n    @tag\n    def m(self):\n        return 1\n";
        let module = compile_src(src).unwrap();
        assert!(module.functions.iter().any(|c| c.name == "C.m"), "method body compiled");
        let ops = &module.body.ops;
        assert!(ops.iter().any(|op| matches!(op, Op::MakeFunction { .. })));
        assert!(ops.iter().any(|op| matches!(op, Op::Call(1))), "the decorator is applied");
        assert!(ops.iter().any(|op| matches!(op, Op::BuildClass)));
    }

    #[test]
    fn class_body_uses_the_namespace_protocol() {
        let module = compile_src("class C:\n    a = 5\n    b = a + 1\n").unwrap();
        let ops = &module.body.ops;
        assert!(ops.iter().any(|op| matches!(op, Op::SetupClassNamespace)));
        assert!(
            ops.iter().filter(|op| matches!(op, Op::StoreName(_))).count() >= 2,
            "a and b are bound with StoreName"
        );
        assert!(ops.iter().any(|op| matches!(op, Op::LoadName(_))), "b = a + 1 reads `a` via LoadName");
        assert!(ops.iter().any(|op| matches!(op, Op::BuildClass)));
        assert!(!ops.iter().any(|op| matches!(op, Op::BuildDict(_))), "the namespace is a register");
        let m = compile_src("g = 1\nclass C:\n    def f(self):\n        return g\n").unwrap();
        let f = func(&m, "C.f");
        assert!(f.ops.iter().any(|op| matches!(op, Op::LoadGlobal(_))), "method read stays global");
        assert!(!f.ops.iter().any(|op| matches!(op, Op::LoadName(_))));
    }

    #[test]
    fn same_named_methods_get_distinct_code_objects() {
        let module = compile_src(
            "class C:\n    @property\n    def x(self):\n        return 1\n    @x.setter\n    def x(self, v):\n        self._x = v\n",
        )
        .unwrap();
        let xs: Vec<&str> = module
            .functions
            .iter()
            .map(|f| f.name.as_str())
            .filter(|n| n.starts_with("C.x"))
            .collect();
        assert_eq!(xs.len(), 2, "two distinct code objects for the two `def x`");
        assert!(xs.contains(&"C.x"));
        assert!(xs.iter().any(|n| n.contains('$')), "the sibling is $-disambiguated");
    }

    #[test]
    fn method_defaults_emit_the_defaults_flags() {
        let m = compile_src("class C:\n    def __init__(self, x=0):\n        self.x = x\n").unwrap();
        assert!(
            m.body
                .ops
                .iter()
                .any(|op| matches!(op, Op::MakeFunction { flags, .. } if flags & 0x01 != 0)),
            "the defaulted __init__ carries the defaults-tuple flag"
        );
        let m2 = compile_src("class C:\n    def m(self, *, k=1):\n        return k\n").unwrap();
        assert!(
            m2.body
                .ops
                .iter()
                .any(|op| matches!(op, Op::MakeFunction { flags, .. } if flags & 0x02 != 0)),
            "the keyword-only method default sets the kwdefaults bit"
        );
    }

    #[test]
    fn multiple_bases_emit_a_bases_tuple() {
        let m = compile_src("class A: pass\nclass B: pass\nclass C(A, B):\n    pass\n").unwrap();
        assert!(
            m.body.ops.iter().any(|op| matches!(op, Op::BuildTuple(2))),
            "the two bases build a 2-tuple"
        );
        let m2 = compile_src("class A: pass\nclass B(A):\n    pass\n").unwrap();
        assert!(
            !m2.body.ops.iter().any(|op| matches!(op, Op::BuildTuple(_))),
            "a single base does not build a bases tuple"
        );
    }

    #[test]
    fn nested_classes_hoist_scope_qualified_methods() {
        let m = compile_src(
            "def make():\n    class Local:\n        def hi(self):\n            return 1\n    return Local()\n",
        )
        .unwrap();
        assert!(
            m.functions.iter().any(|f| f.name == "make.Local.hi"),
            "the method hoists under a scope-qualified name"
        );
        let m2 = compile_src(
            "def a():\n    class Inner:\n        def m(self):\n            return 1\ndef b():\n    class Inner:\n        def m(self):\n            return 2\n",
        )
        .unwrap();
        assert!(m2.functions.iter().any(|f| f.name == "a.Inner.m"));
        assert!(m2.functions.iter().any(|f| f.name == "b.Inner.m"));
        assert!(compile_src("G = 1\ndef f():\n    class C:\n        x = G\n    return C\n").is_ok());
        assert!(compile_src(
            "def f(n):\n    class C:\n        def g(self):\n            return n\n    return C()\n"
        )
        .is_ok());
    }

    #[test]
    fn import_statements_emit_the_import_ops() {
        let m = compile_src("import math\n").unwrap();
        assert!(m.body.ops.iter().any(|op| matches!(op, Op::ImportName(_))));
        let m2 = compile_src("from math import sqrt, pi\n").unwrap();
        let ops = &m2.body.ops;
        assert_eq!(
            ops.iter().filter(|op| matches!(op, Op::ImportName(_))).count(),
            1,
            "one ImportName leads the `from`"
        );
        assert_eq!(
            ops.iter().filter(|op| matches!(op, Op::ImportFrom(_))).count(),
            2,
            "one ImportFrom per imported member"
        );
        assert!(
            ops.iter().any(|op| matches!(op, Op::PopTop)),
            "the module is discarded after the last member"
        );
    }

    #[test]
    fn import_star_emits_importname_then_importstar() {
        let m = compile_src("from math import *\n").unwrap();
        let ops = &m.body.ops;
        let ni = ops.iter().position(|op| matches!(op, Op::ImportName(_)));
        let si = ops.iter().position(|op| matches!(op, Op::ImportStar));
        assert!(ni.is_some() && si.is_some(), "emits ImportName then ImportStar");
        assert!(ni < si, "ImportName (push the module) precedes ImportStar (bind its names)");
        assert!(!ops.iter().any(|op| matches!(op, Op::PopTop)), "ImportStar consumes the module");
        assert!(compile_src("def f():\n    from math import *\n").is_err(), "rejected in a function");
    }

    #[test]
    fn keyword_only_defaults_emit_a_kwdefaults_dict() {
        let m = compile_src("def f(a, *, b=1):\n    return b\n").unwrap();
        let ops = &m.body.ops;
        assert!(ops.iter().any(|op| matches!(op, Op::BuildDict(1))), "the kwdefaults dict");
        assert!(
            ops.iter()
                .any(|op| matches!(op, Op::MakeFunction { flags, .. } if flags & 0x02 != 0)),
            "MakeFunction sets the kwdefaults bit (0x02)"
        );
        let m2 = compile_src("def g(a, b=2, *, c=3):\n    return c\n").unwrap();
        assert!(
            m2.body
                .ops
                .iter()
                .any(|op| matches!(op, Op::MakeFunction { flags, .. } if flags & 0x03 == 0x03)),
            "both the defaults-tuple and kwdefaults-dict bits are set"
        );
    }

    #[test]
    fn stacked_method_decorators_apply_bottom_up() {
        let src = "def a(f):\n    return f\ndef b(f):\n    return f\n\
                   class C:\n    @a\n    @b\n    def m(self):\n        return 1\n";
        let module = compile_src(src).unwrap();
        let calls = module.body.ops.iter().filter(|op| matches!(op, Op::Call(1))).count();
        assert_eq!(calls, 2, "two stacked decorators -> two Call(1)");
    }

    #[test]
    fn lambda_using_only_params_and_globals_is_fine_inside_a_function() {
        let module =
            compile_src("LIMIT = 10\ndef f(xs):\n    g = lambda x: x + LIMIT\n    return g\n")
                .unwrap();
        assert!(module.functions.iter().any(|f| f.name.contains("<lambda.")));
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
    fn tuple_assignment_targets_infer_int_from_a_tuple_literal() {
        let module = compile_src("def f() -> int:\n    a, b = 1, 2\n    return a + b\n").unwrap();
        assert_eq!(
            func(&module, "f").local_types,
            vec![StaticType::Int, StaticType::Int]
        );
    }

    #[test]
    fn tuple_assignment_from_a_non_literal_keeps_targets_dynamic() {
        let module = compile_src("def f(pair) -> int:\n    a, b = pair\n    return 0\n").unwrap();
        assert_eq!(
            func(&module, "f").local_types,
            vec![StaticType::Dynamic, StaticType::Dynamic, StaticType::Dynamic]
        );
    }

    #[test]
    fn unannotated_float_locals_are_inferred() {
        let module =
            compile_src("def f() -> int:\n    x = 3.0\n    y = x + 1.0\n    return 0\n").unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "x").unwrap()], StaticType::Float);
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "y").unwrap()], StaticType::Float);
    }

    #[test]
    fn true_division_and_int_plus_float_infer_float() {
        let module =
            compile_src("def f() -> int:\n    h = 7 / 2\n    m = 5 + 1.0\n    return 0\n").unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "h").unwrap()], StaticType::Float);
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "m").unwrap()], StaticType::Float);
    }

    #[test]
    fn a_mixed_int_and_float_local_is_dynamic() {
        let module =
            compile_src("def f() -> int:\n    x = 1\n    x = 2.0\n    return 0\n").unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "x").unwrap()], StaticType::Dynamic);
    }

    #[test]
    fn a_float_accumulator_over_a_float_loop_infers_float() {
        let module = compile_src(
            "def f() -> int:\n    xs = [1.5, 2.5]\n    s = 0.0\n    for v in xs:\n        s = s + v\n    return int(s)\n",
        )
        .unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "s").unwrap()], StaticType::Float);
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "v").unwrap()], StaticType::Float);
    }

    #[test]
    fn an_int_seeded_then_float_incremented_local_is_dynamic() {
        let module =
            compile_src("def f() -> int:\n    s = 0\n    s = s + 0.5\n    return int(s)\n").unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "s").unwrap()], StaticType::Dynamic);
    }

    #[test]
    fn a_float_comparison_result_infers_int() {
        let module = compile_src(
            "def f() -> int:\n    a = 2.5\n    b = 1.5\n    flag = a > b\n    return 0\n",
        )
        .unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "a").unwrap()], StaticType::Float);
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "flag").unwrap()], StaticType::Int);
    }

    #[test]
    fn a_homogeneous_numeric_list_literal_infers_a_list_type() {
        let module = compile_src(
            "def f() -> int:\n    xs = [1, 2, 3]\n    ys = [1.0, 2.0]\n    return 0\n",
        )
        .unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "xs").unwrap()], StaticType::ListInt);
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "ys").unwrap()], StaticType::ListFloat);
    }

    #[test]
    fn a_list_element_read_and_len_stay_typed() {
        let module = compile_src(
            "def f() -> int:\n    xs = [10, 20, 30]\n    i = 1\n    v = xs[i]\n    n = len(xs)\n    return v + n\n",
        )
        .unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "v").unwrap()], StaticType::Int);
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "n").unwrap()], StaticType::Int);
    }

    #[test]
    fn a_for_loop_over_a_typed_list_infers_the_element_type() {
        let module = compile_src(
            "def f() -> int:\n    xs = [1, 2, 3]\n    for x in xs:\n        pass\n    ys = [1.0, 2.0]\n    for y in ys:\n        pass\n    return 0\n",
        )
        .unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "x").unwrap()], StaticType::Int);
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "y").unwrap()], StaticType::Float);
    }

    #[test]
    fn a_for_loop_over_a_dynamic_iterable_keeps_the_item_dynamic() {
        let module = compile_src("def f(s) -> int:\n    for ch in s:\n        pass\n    return 0\n").unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "ch").unwrap()], StaticType::Dynamic);
    }

    #[test]
    fn a_float_list_element_read_infers_float() {
        let module = compile_src(
            "def f() -> int:\n    xs = [1.5, 2.5]\n    s = xs[0]\n    return 0\n",
        )
        .unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "s").unwrap()], StaticType::Float);
    }

    #[test]
    fn a_mixed_or_empty_list_literal_is_dynamic() {
        let module = compile_src(
            "def f() -> int:\n    a = [1, 2.0]\n    b = []\n    return 0\n",
        )
        .unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "a").unwrap()], StaticType::Dynamic);
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "b").unwrap()], StaticType::Dynamic);
    }

    #[test]
    fn a_homogeneous_numeric_tuple_literal_infers_a_tuple_type() {
        let module = compile_src(
            "def f() -> int:\n    t = (1, 2, 3)\n    u = (1.0, 2.0)\n    a = 5\n    b = 7\n    v = (a, b)\n    return 0\n",
        )
        .unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "t").unwrap()], StaticType::TupleInt);
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "u").unwrap()], StaticType::TupleFloat);
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "v").unwrap()], StaticType::TupleInt);
    }

    #[test]
    fn a_tuple_element_read_stays_typed() {
        let module = compile_src(
            "def f() -> int:\n    t = (10, 20, 30)\n    v = t[1]\n    return 0\n",
        )
        .unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "v").unwrap()], StaticType::Int);
    }

    #[test]
    fn a_for_loop_over_a_typed_tuple_infers_the_element_type() {
        let module = compile_src(
            "def f() -> int:\n    for x in (1, 2, 3):\n        pass\n    return 0\n",
        )
        .unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "x").unwrap()], StaticType::Int);
    }

    #[test]
    fn a_mixed_tuple_literal_is_dynamic() {
        let module = compile_src("def f() -> int:\n    t = (1, 2.0)\n    return 0\n").unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "t").unwrap()], StaticType::Dynamic);
    }

    #[test]
    fn a_parallel_assignment_is_not_a_tuple_type() {
        let module = compile_src("def f() -> int:\n    a, b = 1, 2\n    return a + b\n").unwrap();
        let f = func(&module, "f");
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "a").unwrap()], StaticType::Int);
        assert_eq!(f.local_types[f.local_names.iter().position(|n| n == "b").unwrap()], StaticType::Int);
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
            m.body.ops.iter().filter(|op| matches!(op, Op::MakeFunction { .. })).count(),
            3
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
    fn except_handler_clears_the_exception_after_its_body() {
        let src = "def f():\n    try:\n        pass\n    except ValueError:\n        x = 1\n        raise\n";
        let module = compile_src(src).unwrap();
        let co = func(&module, "f");
        let store = co.ops.iter().position(|op| matches!(op, Op::StoreFast(_)))
            .expect("the handler body stores x");
        let pop = co.ops.iter().position(|op| matches!(op, Op::PopExcept))
            .expect("a PopExcept");
        assert!(store < pop, "PopExcept must come after the handler body, not before");
        assert!(co.ops.iter().any(|op| matches!(op, Op::Raise(0))));
    }

    #[test]
    fn break_out_of_a_handler_clears_the_exception() {
        let src = "def f():\n    while True:\n        try:\n            raise E\n        except E:\n            break\n";
        let module = compile_src(src).unwrap();
        let co = func(&module, "f");
        let pops = co.ops.iter().filter(|op| matches!(op, Op::PopExcept)).count();
        assert_eq!(pops, 2, "break out of a handler emits a PopExcept before the loop-exit jump");
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
    fn starred_assign_emits_unpack_ex() {
        let m = compile_src("def f(p):\n    a, *b = p\n    return a\n").unwrap();
        assert!(func(&m, "f").ops.iter().any(|op| matches!(op, Op::UnpackEx { before: 1, after: 0 })));
        let m2 = compile_src("def f(p):\n    a, *b, c = p\n    return a\n").unwrap();
        assert!(func(&m2, "f").ops.iter().any(|op| matches!(op, Op::UnpackEx { before: 1, after: 1 })));
    }

    #[test]
    fn sets_emit_buildset_and_setadd() {
        let m = compile_src("def f():\n    return {1, 2, 3}\n").unwrap();
        assert!(func(&m, "f").ops.iter().any(|op| matches!(op, Op::BuildSet(3))));
        let c = compile_src("def f(r):\n    return {x for x in r}\n").unwrap();
        let comp = c
            .functions
            .iter()
            .find(|co| co.name.contains("<setcomp."))
            .expect("a hoisted setcomp function");
        assert!(comp.ops.iter().any(|op| matches!(op, Op::BuildSet(0))));
        assert!(comp.ops.iter().any(|op| matches!(op, Op::ForIter(_))));
        assert!(func(&c, "f").ops.iter().any(|op| matches!(op, Op::Call(1))));
        assert!(!func(&c, "f").ops.iter().any(|op| matches!(op, Op::BuildSet(_))));
    }

    #[test]
    fn comprehensions_hoist_and_build_in_their_own_function() {
        let m = compile_src("def f(r):\n    return [x * 2 for x in r if x]\n").unwrap();
        let comp = m
            .functions
            .iter()
            .find(|co| co.name.contains("<listcomp."))
            .expect("a hoisted listcomp function");
        assert!(comp.ops.iter().any(|op| matches!(op, Op::BuildList(0))));
        assert!(comp.ops.iter().any(|op| matches!(op, Op::ForIter(_))));
        assert!(comp.ops.iter().any(|op| matches!(op, Op::Return)));
        assert!(func(&m, "f").ops.iter().any(|op| matches!(op, Op::Call(1))));
        assert!(!func(&m, "f").ops.iter().any(|op| matches!(op, Op::BuildList(0))));
        let d = compile_src("def f(r):\n    return {x: x for x in r}\n").unwrap();
        let dc = d
            .functions
            .iter()
            .find(|co| co.name.contains("<dictcomp."))
            .expect("a hoisted dictcomp function");
        assert!(dc.ops.iter().any(|op| matches!(op, Op::BuildDict(0))));
        assert!(dc.ops.iter().any(|op| matches!(op, Op::Setitem)));
    }

    #[test]
    fn a_list_display_emits_buildlist() {
        let m = compile_src("def f(a, b):\n    return [a, b, a]\n").unwrap();
        let f = func(&m, "f");
        assert!(f.ops.iter().any(|op| matches!(op, Op::BuildList(3))));
    }

    #[test]
    fn bare_name_augmented_assign_emits_inplace_binop() {
        let m = compile_src("def f(x):\n    x += 1\n    return x\n").unwrap();
        let ops = &func(&m, "f").ops;
        assert!(ops.iter().any(|op| matches!(op, Op::InplaceBinOp(bc::BinOp::Add))));
        assert!(!ops.iter().any(|op| matches!(op, Op::Binary(bc::BinOp::Add))), "not a plain binary");
        let p = compile_src("def g(x):\n    return x + 1\n").unwrap();
        assert!(func(&p, "g").ops.iter().any(|op| matches!(op, Op::Binary(bc::BinOp::Add))));
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
