//! Lowering the AST to our bytecode.

use alloc::string::String;
use alloc::vec::Vec;

use lamella_py_bytecode as bc;

use crate::ast::{self, Assign, Expr, FuncDef, ModuleAst, Stmt};

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
        Stmt::Return(_) | Stmt::Expr(_) | Stmt::FuncDef(_) => {}
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
                    Some(expr) => self.compile_expr(expr)?,
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
                self.compile_expr(expr)?;
                self.asm.emit(bc::Op::PopTop);
                Ok(())
            }
            Stmt::If { test, body, orelse } => self.compile_if(test, body, orelse),
            Stmt::While { test, body } => self.compile_while(test, body),
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

    fn compile_while(&mut self, test: &Expr, body: &[Stmt]) -> Result<(), CompileError> {
        let top_label = self.asm.new_label();
        self.asm.place(top_label);
        self.compile_expr(test)?;
        let end_label = self.asm.new_label();
        self.asm.emit_branch(end_label);
        for stmt in body {
            self.compile_stmt(stmt)?;
        }
        self.asm.emit_jump(top_label);
        self.asm.place(end_label);
        Ok(())
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
}

fn binop_sel(op: ast::BinOp) -> bc::BinOp {
    match op {
        ast::BinOp::Add => bc::BinOp::Add,
        ast::BinOp::Sub => bc::BinOp::Sub,
        ast::BinOp::Mul => bc::BinOp::Mul,
        ast::BinOp::FloorDiv => bc::BinOp::FloorDiv,
        ast::BinOp::Mod => bc::BinOp::Mod,
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
