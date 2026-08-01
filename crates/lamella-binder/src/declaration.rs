//! Collecting the types and members declared in source (ECMA-334 1st ed,
//! clauses 16-18).

use crate::bind::{bind_type, parameter_symbol};
use crate::bound::{coerce_constant, integer_literal, literal_int_value};
use lamella_syntax::token::{IntegerSuffix, RealSuffix};
use crate::resolve::TypeTable;
use crate::special::SpecialType;
use crate::symbols::{
    Accessibility, EventSymbol, FieldSymbol, MethodSymbol, Model, PropertySymbol, TypeInfo,
    TypeKind,
};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_syntax::ast::{
    AttributeArgument, AttributeSection, BinaryOperator, CompilationUnit, Expr, ExprKind, Literal,
    Member, Modifier, NamespaceMember, QualifiedName, TypeDecl, TypeKind as SyntaxTypeKind,
    UnaryOperator, explicit_interface_member_name,
};

/// Builds the [`Model`] of every type and member declared in `unit`.
#[must_use]
pub fn collect_model(unit: &CompilationUnit) -> Model {
    let mut model = Model::new();
    collect_into(&mut model, unit);
    model.link_bases();
    model
}

/// Adds `unit`'s declared types to an existing model (e.g. one already holding the
/// reference assemblies). The caller links bases once the model is complete.
pub fn collect_into(model: &mut Model, unit: &CompilationUnit) {
    for member in &unit.members {
        collect_namespace_member(member, "", model);
    }
    model.default_toplevel_types_to_internal();
}

/// Builds a [`TypeTable`] of every type declared in `unit` (the existence-only
/// view derived from the full [`Model`]).
#[must_use]
pub fn collect_types(unit: &CompilationUnit) -> TypeTable {
    collect_model(unit).type_table()
}

fn collect_namespace_member(member: &NamespaceMember, namespace: &str, model: &mut Model) {
    match member {
        NamespaceMember::Namespace(declaration) => {
            let inner = join_namespace(namespace, &declaration.name);
            for inner_member in &declaration.members {
                collect_namespace_member(inner_member, &inner, model);
            }
        }
        NamespaceMember::Type(declaration) => {
            model.insert(type_info(namespace, declaration));
            collect_nested_types(declaration, namespace, model);
        }
        NamespaceMember::Enum(declaration) => {
            let mut info = TypeInfo::new(namespace, &declaration.name, TypeKind::Enum);
            info.accessibility = accessibility_of(&declaration.modifiers);
            let enum_base = named_symbol("System", "Enum");
            info.bases.push(enum_base.clone());
            info.base = Some(enum_base);
            let enum_ty = named_symbol(namespace, &declaration.name);
            let mut next_value: i64 = 0;
            let mut prior: BTreeMap<Box<str>, i64> = BTreeMap::new();
            for member in &declaration.members {
                let value = member
                    .value
                    .as_ref()
                    .and_then(|expr| eval_enum_member(expr, &prior))
                    .unwrap_or(next_value);
                next_value = value.wrapping_add(1);
                prior.insert(member.name.clone(), value);
                info.fields.push(FieldSymbol {
                    name: member.name.clone(),
                    ty: enum_ty.clone(),
                    is_static: true,
                    is_readonly: false,
                    is_volatile: false,
                    accessibility: Accessibility::Public,
                    constant: Some(integer_literal(value)),
                });
            }
            model.insert(info);
        }
        NamespaceMember::Delegate(declaration) => {
            let mut info = TypeInfo::new(namespace, &declaration.name, TypeKind::Delegate);
            info.accessibility = accessibility_of(&declaration.modifiers);
            info.methods.push(MethodSymbol {
                name: "Invoke".into(),
                return_type: bind_type(&declaration.return_type),
                parameters: declaration
                    .parameters
                    .iter()
                    .map(parameter_symbol)
                    .collect(),
                parameter_info: crate::bind::parameter_infos(&declaration.parameters),
                is_static: false,
                is_params: has_params_array(&declaration.parameters),
                is_vararg: false,
                is_virtual: false,
                is_abstract: false,
                is_override: false,
                is_sealed: false,
                accessibility: Accessibility::Public,
                conditional: Vec::new(),
            });
            model.insert(info);
        }
    }
}

/// Collects the class/struct types nested in `declaration`, each keyed under the
/// enclosing type's full name (so `Outer.Inner` resolves to it) and marked with its
/// enclosing type (driving the `NestedClass` row + empty namespace at emission). Recurses
/// for deeper nesting. Nested enums/delegates are a follow-up.
fn collect_nested_types(declaration: &TypeDecl, namespace: &str, model: &mut Model) {
    let enclosing_full = qualified_type_name(namespace, &declaration.name);
    for member in &declaration.members {
        if let Member::NestedType(nested) = member {
            collect_namespace_member(nested, &enclosing_full, model);
            if let Some(name) = nested_member_name(nested) {
                model.set_enclosing(&enclosing_full, name, &enclosing_full);
            }
        }
    }
}

/// The simple name of a nested type member (a class/struct/interface/enum/delegate).
fn nested_member_name(member: &NamespaceMember) -> Option<&str> {
    match member {
        NamespaceMember::Type(declaration) => Some(declaration.name.as_ref()),
        NamespaceMember::Enum(declaration) => Some(declaration.name.as_ref()),
        NamespaceMember::Delegate(declaration) => Some(declaration.name.as_ref()),
        NamespaceMember::Namespace(_) => None,
    }
}

/// Joins a namespace (possibly empty) and a simple name into a dotted full name.
pub(crate) fn qualified_type_name(namespace: &str, name: &str) -> alloc::string::String {
    if namespace.is_empty() {
        alloc::string::String::from(name)
    } else {
        alloc::format!("{namespace}.{name}")
    }
}

/// A `const` field's folded value (14.15): its constant-expression initializer, with a reference
/// to a `prior` const field (declared earlier in the same type) resolved to that field's value --
/// so `const B = A;` and `const C = A + 1;` fold, not just literal arithmetic. `None` when it is
/// not a compile-time constant, so the field stays a runtime field at the use site.
fn const_field_literal(expr: &Expr, prior: &BTreeMap<Box<str>, Literal>) -> Option<Literal> {
    fold_const(expr, &|name| prior.get(name).cloned())
}

/// An enum member's underlying value (21.4): its initializer folded as a constant expression,
/// with references to `prior` members resolved to their assigned values. `None` when the
/// initializer is not a constant expression, so the caller continues the auto-increment.
fn eval_enum_member(expr: &Expr, prior: &BTreeMap<Box<str>, i64>) -> Option<i64> {
    let value = fold_const(expr, &|name| prior.get(name).copied().map(integer_literal))?;
    literal_int_value(&value)
}

/// Folds a constant expression (14.15) to its [`Literal`] value: a literal, a parenthesized
/// expression, a name resolved by `lookup` (an enum member; nothing for a field), a unary or
/// binary operation, a numeric/char cast, or a conditional whose condition folds to a `bool`.
/// `None` for anything not a compile-time constant.
fn fold_const(expr: &Expr, lookup: &dyn Fn(&str) -> Option<Literal>) -> Option<Literal> {
    match &expr.kind {
        ExprKind::Literal(literal) => Some(literal.clone()),
        ExprKind::Parenthesized(inner) => fold_const(inner, lookup),
        ExprKind::Name(name) => lookup(name),
        ExprKind::Unary { operator, operand } => {
            fold_const_unary(*operator, &fold_const(operand, lookup)?)
        }
        ExprKind::Binary {
            operator,
            left,
            right,
        } => fold_const_binary(
            *operator,
            &fold_const(left, lookup)?,
            &fold_const(right, lookup)?,
        ),
        ExprKind::Cast { target, operand } => {
            let operand = fold_const(operand, lookup)?;
            match bind_type(target) {
                TypeSymbol::Special(special) => coerce_constant(literal_int_value(&operand)?, special),
                _ => None,
            }
        }
        ExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => match fold_const(condition, lookup)? {
            Literal::Boolean(true) => fold_const(when_true, lookup),
            Literal::Boolean(false) => fold_const(when_false, lookup),
            _ => None,
        },
        _ => None,
    }
}

/// Collects the bare `Name` operands a constant-expression references, walking the same forms as
/// [`fold_const`]. Used to build the const-reference graph for CS0110 circular-constant detection.
pub(crate) fn const_expr_references(expr: &Expr, out: &mut Vec<Box<str>>) {
    match &expr.kind {
        ExprKind::Name(name) => out.push(name.clone()),
        ExprKind::Parenthesized(inner) => const_expr_references(inner, out),
        ExprKind::Unary { operand, .. } => const_expr_references(operand, out),
        ExprKind::Binary { left, right, .. } => {
            const_expr_references(left, out);
            const_expr_references(right, out);
        }
        ExprKind::Cast { operand, .. } => const_expr_references(operand, out),
        ExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => {
            const_expr_references(condition, out);
            const_expr_references(when_true, out);
            const_expr_references(when_false, out);
        }
        _ => {}
    }
}

/// Folds a unary operator applied to an already-folded constant operand (14.6).
pub(crate) fn fold_const_unary(operator: UnaryOperator, operand: &Literal) -> Option<Literal> {
    match operator {
        UnaryOperator::Plus => Some(operand.clone()),
        UnaryOperator::Minus => match operand {
            Literal::Integer {
                value: 9_223_372_036_854_775_808,
                suffix: IntegerSuffix::None,
            } => Some(Literal::Integer {
                value: 9_223_372_036_854_775_808,
                suffix: IntegerSuffix::Long,
            }),
            Literal::Real { bits, suffix } => Some(Literal::Real {
                bits: (-f64::from_bits(*bits)).to_bits(),
                suffix: *suffix,
            }),
            _ => Some(integer_literal(literal_int_value(operand)?.checked_neg()?)),
        },
        UnaryOperator::Complement => Some(integer_literal(!literal_int_value(operand)?)),
        UnaryOperator::Not => match operand {
            Literal::Boolean(value) => Some(Literal::Boolean(!value)),
            _ => None,
        },
        _ => None,
    }
}

/// Folds a binary operator applied to two already-folded constant operands (14.7-14.12). Integer
/// arithmetic uses checked ops, so an overflow or a divide-by-zero declines to fold (it is not a
/// constant value); string `+` concatenates. Operands are treated as `i64`, so a `const` whose type
/// or value needs the operand's own width (e.g. a `uint` expression near `u32::MAX`) is not folded.
/// The f64 value of a numeric constant literal -- a real's stored bits, or an integer widened to
/// double -- for folding floating-point constant arithmetic.
fn real_value(literal: &Literal) -> Option<f64> {
    match literal {
        Literal::Real { bits, .. } => Some(f64::from_bits(*bits)),
        Literal::Integer { .. } => Some(literal_int_value(literal)? as f64),
        _ => None,
    }
}

/// Whether a numeric literal is a `double` -- a real with no suffix or the `d`/`D` suffix. Binary
/// numeric promotion (14.7.2) makes an arithmetic result double when either operand is one.
fn is_double_literal(literal: &Literal) -> bool {
    matches!(
        literal,
        Literal::Real {
            suffix: RealSuffix::None | RealSuffix::Double,
            ..
        }
    )
}

pub(crate) fn fold_const_binary(
    operator: BinaryOperator,
    left: &Literal,
    right: &Literal,
) -> Option<Literal> {
    use BinaryOperator as Op;
    if operator == Op::Add
        && (matches!(left, Literal::String(_)) || matches!(right, Literal::String(_)))
    {
        let units = |lit: &Literal| match lit {
            Literal::String(s) => Some(s.to_vec()),
            Literal::Null => Some(Vec::new()),
            _ => None,
        };
        if let (Some(mut concatenated), Some(tail)) = (units(left), units(right)) {
            concatenated.extend_from_slice(&tail);
            return Some(Literal::String(concatenated.into()));
        }
    }
    if matches!(left, Literal::Real { .. }) || matches!(right, Literal::Real { .. }) {
        let (lv, rv) = (real_value(left)?, real_value(right)?);
        let result = match operator {
            Op::Add => lv + rv,
            Op::Subtract => lv - rv,
            Op::Multiply => lv * rv,
            Op::Divide => lv / rv,
            Op::Modulo => lv % rv,
            Op::LessThan => return Some(Literal::Boolean(lv < rv)),
            Op::GreaterThan => return Some(Literal::Boolean(lv > rv)),
            Op::LessThanOrEqual => return Some(Literal::Boolean(lv <= rv)),
            Op::GreaterThanOrEqual => return Some(Literal::Boolean(lv >= rv)),
            Op::Equal => return Some(Literal::Boolean(lv == rv)),
            Op::NotEqual => return Some(Literal::Boolean(lv != rv)),
            _ => return None,
        };
        let (result, suffix) = if is_double_literal(left) || is_double_literal(right) {
            (result, RealSuffix::Double)
        } else {
            (f64::from(result as f32), RealSuffix::Float)
        };
        return Some(Literal::Real {
            bits: result.to_bits(),
            suffix,
        });
    }
    let (left, right) = (literal_int_value(left)?, literal_int_value(right)?);
    let value = match operator {
        Op::Multiply => left.checked_mul(right)?,
        Op::Divide => left.checked_div(right)?,
        Op::Modulo => left.checked_rem(right)?,
        Op::Add => left.checked_add(right)?,
        Op::Subtract => left.checked_sub(right)?,
        Op::LeftShift => left.checked_shl(u32::try_from(right).ok()?)?,
        Op::RightShift => left.checked_shr(u32::try_from(right).ok()?)?,
        Op::BitwiseAnd => left & right,
        Op::BitwiseXor => left ^ right,
        Op::BitwiseOr => left | right,
        Op::LessThan => return Some(Literal::Boolean(left < right)),
        Op::GreaterThan => return Some(Literal::Boolean(left > right)),
        Op::LessThanOrEqual => return Some(Literal::Boolean(left <= right)),
        Op::GreaterThanOrEqual => return Some(Literal::Boolean(left >= right)),
        Op::Equal => return Some(Literal::Boolean(left == right)),
        Op::NotEqual => return Some(Literal::Boolean(left != right)),
        Op::LogicalAnd => return Some(Literal::Boolean(left != 0 && right != 0)),
        Op::LogicalOr => return Some(Literal::Boolean(left != 0 || right != 0)),
    };
    Some(integer_literal(value))
}

/// A `const` field awaiting model-aware resolution: its containing type's full name, its own name,
/// and its initializer expression (borrowed from the AST).
struct ConstDecl<'a> {
    type_full: String,
    name: &'a str,
    init: &'a Expr,
}

/// Model-aware, dependency-ordered constant resolution (14.15). A second pass that folds the
/// `const` fields the declaration-order pass in [`type_info`] left unresolved -- a FORWARD reference
/// (`const A = B; const B = 42;`) or a QUALIFIED reference (`const A = Other.Value;`), each of which
/// needs the whole model rather than only the earlier same-type consts that pass can see. It is
/// STRICTLY ADDITIVE: it fills a field only when its constant is still `None` and its initializer
/// folds to a value against the fully collected model -- so it can neither change a value the first
/// pass already resolved nor fold a non-constant initializer. A reference cycle simply never makes
/// progress, so those fields stay `None` (a runtime field, as today) and the loop terminates. Enum
/// members are excluded: their unresolved case is auto-numbering, a separate concern.
pub fn resolve_constants(model: &mut Model, units: &[CompilationUnit]) {
    let mut pending: Vec<ConstDecl> = Vec::new();
    for unit in units {
        for member in &unit.members {
            collect_const_field_decls(member, "", &mut pending);
        }
    }
    if pending.is_empty() {
        return;
    }
    let mut values = model_const_values(model);
    loop {
        let mut progress = false;
        for decl in &pending {
            let key = (decl.type_full.clone(), decl.name.to_string());
            if values.contains_key(&key) {
                continue;
            }
            if let Some(literal) = resolve_const_expr(decl.init, &decl.type_full, &values) {
                values.insert(key, literal);
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    resolve_enum_members(model, units, &values);
    for decl in &pending {
        let Some(literal) = values.get(&(decl.type_full.clone(), decl.name.to_string())) else {
            continue;
        };
        let (namespace, name) = split_type_full(&decl.type_full);
        if let Some(info) = model.get_mut(&namespace, name) {
            if let Some(field) = info.fields.iter_mut().find(|field| &*field.name == decl.name) {
                if field.constant.is_none() {
                    field.constant = Some(literal.clone());
                }
            }
        }
    }
}

/// Re-numbers every enum against the WHOLE model, which the first pass could not do.
///
/// THE FAILURE THIS FIXES IS SILENT BY CONSTRUCTION. The first pass evaluates a member's
/// initializer against `prior` -- the enum's own earlier members -- and falls back to auto-numbering
/// when that fails. So `Normal = (int)Facts.MODE_NORMAL` does not resolve, takes the auto value, and
/// the enum compiles and runs with the WRONG constant: an unresolvable initializer is
/// indistinguishable from an absent one, because both end at the same `unwrap_or`.
///
/// AND IT HAS NO WORKAROUND, which is why it is worth a second pass of its own. A const FIELD that
/// mis-folds can be spelled `static readonly` and computed at run time; an enum member must be a
/// compile-time constant, so a driver naming a generated device code had to transcribe it by hand --
/// the exact duplication the generated facts tables exist to remove.
///
/// The whole enum is re-numbered rather than patched member-by-member, because a late-resolving
/// member moves every auto-numbered member after it (`enum E { A = Facts.X, B }` -- B is X+1). Same
/// order of precedence as the first pass: the enum's own earlier members win over the model, so a
/// same-enum reference cannot be captured by a same-named constant elsewhere.
fn resolve_enum_members(
    model: &mut Model,
    units: &[CompilationUnit],
    values: &BTreeMap<(String, String), Literal>,
) {
    let mut enums: Vec<(String, &str, &[lamella_syntax::ast::EnumMember])> = Vec::new();
    for unit in units {
        for member in &unit.members {
            collect_enum_decls(member, "", &mut enums);
        }
    }
    for (namespace, name, members) in enums {
        let full = qualified_type_name(&namespace, name);
        let mut next_value: i64 = 0;
        let mut prior: BTreeMap<Box<str>, i64> = BTreeMap::new();
        let mut renumbered: Vec<(Box<str>, i64)> = Vec::new();
        for member in members {
            let value = member
                .value
                .as_ref()
                .and_then(|expr| {
                    eval_enum_member(expr, &prior).or_else(|| {
                        resolve_const_expr(expr, &full, values).as_ref().and_then(literal_int_value)
                    })
                })
                .unwrap_or(next_value);
            next_value = value.wrapping_add(1);
            prior.insert(member.name.clone(), value);
            renumbered.push((member.name.clone(), value));
        }
        let (namespace, name) = split_type_full(&full);
        let Some(info) = model.get_mut(&namespace, name) else {
            continue;
        };
        for (member_name, value) in renumbered {
            if let Some(field) = info.fields.iter_mut().find(|f| f.name == member_name) {
                field.constant = Some(integer_literal(value));
            }
        }
    }
}

/// Collects every enum declaration, descending namespaces and nested types, with its containing
/// namespace (or enclosing type's full name, for a nested enum).
fn collect_enum_decls<'a>(
    member: &'a NamespaceMember,
    namespace: &str,
    out: &mut Vec<(String, &'a str, &'a [lamella_syntax::ast::EnumMember])>,
) {
    match member {
        NamespaceMember::Namespace(declaration) => {
            let inner = join_namespace(namespace, &declaration.name);
            for nested in &declaration.members {
                collect_enum_decls(nested, &inner, out);
            }
        }
        NamespaceMember::Enum(declaration) => {
            out.push((namespace.to_string(), &declaration.name, &declaration.members));
        }
        NamespaceMember::Type(declaration) => {
            let full = qualified_type_name(namespace, &declaration.name);
            for member in &declaration.members {
                if let Member::NestedType(nested) = member {
                    collect_enum_decls(nested, &full, out);
                }
            }
        }
        NamespaceMember::Delegate(_) => {}
    }
}

/// Collects the `const` fields (with initializers) declared in `member`, descending namespaces and
/// nested types, each keyed by its containing type's full name.
fn collect_const_field_decls<'a>(
    member: &'a NamespaceMember,
    namespace: &str,
    out: &mut Vec<ConstDecl<'a>>,
) {
    match member {
        NamespaceMember::Namespace(declaration) => {
            let inner = join_namespace(namespace, &declaration.name);
            for nested in &declaration.members {
                collect_const_field_decls(nested, &inner, out);
            }
        }
        NamespaceMember::Type(declaration) => {
            let full = qualified_type_name(namespace, &declaration.name);
            for member in &declaration.members {
                match member {
                    Member::Field {
                        modifiers,
                        declarators,
                        ..
                    } if modifiers.iter().any(|m| matches!(m, Modifier::Const)) => {
                        for declarator in declarators {
                            if let Some(init) = &declarator.initializer {
                                out.push(ConstDecl {
                                    type_full: full.clone(),
                                    name: &declarator.name,
                                    init,
                                });
                            }
                        }
                    }
                    Member::NestedType(nested) => collect_const_field_decls(nested, &full, out),
                    _ => {}
                }
            }
        }
        NamespaceMember::Enum(_) | NamespaceMember::Delegate(_) => {}
    }
}

/// Folds a constant expression against `values` -- the constants collected across the whole model --
/// so a simple name resolves to a `const`/enum member of `containing`, and a qualified `Type.Member`
/// resolves against the named type. Mirrors [`fold_const`]'s operator/cast/conditional handling.
pub(crate) fn resolve_const_expr(
    expr: &Expr,
    containing: &str,
    values: &BTreeMap<(String, String), Literal>,
) -> Option<Literal> {
    match &expr.kind {
        ExprKind::Literal(literal) => Some(literal.clone()),
        ExprKind::Parenthesized(inner) => resolve_const_expr(inner, containing, values),
        ExprKind::Name(name) => values
            .get(&(containing.to_string(), name.to_string()))
            .cloned(),
        ExprKind::MemberAccess { receiver, name } => {
            let type_full = dotted_name(receiver)?;
            values.get(&(type_full, name.to_string())).cloned()
        }
        ExprKind::Unary { operator, operand } => {
            fold_const_unary(*operator, &resolve_const_expr(operand, containing, values)?)
        }
        ExprKind::Binary {
            operator,
            left,
            right,
        } => fold_const_binary(
            *operator,
            &resolve_const_expr(left, containing, values)?,
            &resolve_const_expr(right, containing, values)?,
        ),
        ExprKind::Cast { target, operand } => {
            let operand = resolve_const_expr(operand, containing, values)?;
            match bind_type(target) {
                TypeSymbol::Special(special) => {
                    coerce_constant(literal_int_value(&operand)?, special)
                }
                _ => None,
            }
        }
        ExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => match resolve_const_expr(condition, containing, values)? {
            Literal::Boolean(true) => resolve_const_expr(when_true, containing, values),
            Literal::Boolean(false) => resolve_const_expr(when_false, containing, values),
            _ => None,
        },
        _ => None,
    }
}

/// The dotted name of a chain of simple-name/member-access expressions (`A`, `A.B`, `A.B.C`), or
/// `None` for anything else -- used to read the type name out of a qualified constant reference.
fn dotted_name(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Name(name) => Some(name.to_string()),
        ExprKind::MemberAccess { receiver, name } => {
            Some(alloc::format!("{}.{}", dotted_name(receiver)?, name))
        }
        _ => None,
    }
}

/// Splits a type's full name into its model key: the namespace-or-enclosing-type prefix and the
/// simple name (`Ns.Sub.T` -> (`Ns.Sub`, `T`); `T` -> (``, `T`)).
fn split_type_full(full: &str) -> (String, &str) {
    match full.rsplit_once('.') {
        Some((prefix, name)) => (String::from(prefix), name),
        None => (String::new(), full),
    }
}

/// Every folded constant in the model, keyed by (containing type's full name, member name) -- const
/// fields AND enum members. The lookup table the model-aware constant folder resolves names against.
pub(crate) fn model_const_values(model: &Model) -> BTreeMap<(String, String), Literal> {
    let mut values: BTreeMap<(String, String), Literal> = BTreeMap::new();
    let keys: Vec<(String, String)> = model
        .type_keys()
        .map(|(namespace, name)| (String::from(namespace), String::from(name)))
        .collect();
    for (namespace, name) in &keys {
        let Some(info) = model.get(namespace, name) else {
            continue;
        };
        let full = qualified_type_name(namespace, name);
        for field in &info.fields {
            if let Some(literal) = &field.constant {
                values.insert((full.clone(), field.name.to_string()), literal.clone());
            }
        }
    }
    register_unambiguous_short_spellings(&keys, &mut values);
    values
}

/// Also registers each constant under the SHORTER type spellings a source file may legitimately
/// write -- `Facts.X` for `G.Facts.X` under a `using G;`, `Outer.Inner.X` for `Ns.Outer.Inner.X`.
///
/// WHY THIS IS NEEDED AT ALL: a constant initializer is folded against this map by NAME, using the
/// receiver exactly as the source wrote it. A `using`-shortened receiver therefore matched nothing,
/// and the fold quietly produced no value -- which downstream is indistinguishable from zero. That
/// is the const-of-const mis-fold: `const byte X = (byte)Facts.IDENTITY_REG;` read back as 0 while
/// the identical expression written INLINE folded correctly, because the inline path resolves names
/// through scope and this one did not.
///
/// ONLY UNAMBIGUOUS SUFFIXES ARE REGISTERED, and that is the whole safety argument. This is not
/// scope resolution -- it does not know which namespaces a file imported -- so it earns the right
/// to answer only where every candidate agrees. A suffix owned by two types resolves to NOTHING,
/// which leaves the fold unresolved exactly as it is today: a gap, never a wrong value. A file that
/// writes `Facts.X` while two `Facts` types exist gets no fold rather than the wrong one.
///
/// A full name already inserted above is never displaced -- aliases fill empty slots only.
fn register_unambiguous_short_spellings(
    keys: &[(String, String)],
    values: &mut BTreeMap<(String, String), Literal>,
) {
    let full_names: Vec<String> = keys
        .iter()
        .map(|(namespace, name)| qualified_type_name(namespace, name))
        .collect();
    let mut owners: BTreeMap<String, usize> = BTreeMap::new();
    for full in &full_names {
        for suffix in dotted_suffixes(full) {
            *owners.entry(suffix).or_insert(0) += 1;
        }
    }
    let constants: Vec<((String, String), Literal)> = values
        .iter()
        .map(|(key, literal)| (key.clone(), literal.clone()))
        .collect();
    for full in &full_names {
        for suffix in dotted_suffixes(full) {
            if owners.get(&suffix).copied() != Some(1) {
                continue;
            }
            for ((owner, field), literal) in &constants {
                if owner == full {
                    values
                        .entry((suffix.clone(), field.clone()))
                        .or_insert_with(|| literal.clone());
                }
            }
        }
    }
}

/// The proper dotted suffixes of a type's full name -- `A.B.C` yields `B.C` and `C`. The full name
/// itself is excluded: it is already a key.
fn dotted_suffixes(full: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = full;
    while let Some((_, tail)) = rest.split_once('.') {
        out.push(tail.to_string());
        rest = tail;
    }
    out
}

/// Whether `expr` is a constant-expression FORM (14.15): built only from literals, names, member
/// accesses, and the constant operators / casts / conditionals -- never a method call, object
/// creation, element access, increment, or other form that can never be a compile-time constant. A
/// `true` result does NOT prove it folds (a name may be unresolved -- that is a separate CS0103); a
/// `false` result means the SHAPE alone rules it out, so the use site is non-constant.
pub(crate) fn is_constant_form(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Literal(_) | ExprKind::Name(_) | ExprKind::PredefinedType(_) => true,
        ExprKind::Parenthesized(inner) => is_constant_form(inner),
        ExprKind::MemberAccess { receiver, .. } => is_constant_form(receiver),
        ExprKind::Unary { operator, operand } => {
            matches!(
                operator,
                UnaryOperator::Plus
                    | UnaryOperator::Minus
                    | UnaryOperator::Not
                    | UnaryOperator::Complement
            ) && is_constant_form(operand)
        }
        ExprKind::Binary { left, right, .. } => is_constant_form(left) && is_constant_form(right),
        ExprKind::Cast { operand, .. } => is_constant_form(operand),
        ExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => {
            is_constant_form(condition)
                && is_constant_form(when_true)
                && is_constant_form(when_false)
        }
        _ => false,
    }
}

/// Builds the [`TypeInfo`] for one type declaration, collecting its fields and
/// methods.
fn type_info(namespace: &str, declaration: &TypeDecl) -> TypeInfo {
    let mut info = TypeInfo::new(namespace, &declaration.name, map_kind(declaration.kind));
    info.accessibility = accessibility_of(&declaration.modifiers);
    info.is_sealed = declaration.modifiers.iter().any(|m| matches!(m, Modifier::Sealed))
        || matches!(declaration.kind, SyntaxTypeKind::Struct);
    info.is_abstract = declaration
        .modifiers
        .iter()
        .any(|m| matches!(m, Modifier::Abstract))
        || matches!(declaration.kind, SyntaxTypeKind::Interface);
    info.bases = declaration.bases.iter().map(bind_type).collect();
    if matches!(declaration.kind, SyntaxTypeKind::Struct) {
        info.bases.push(named_symbol("System", "ValueType"));
    }
    if matches!(declaration.kind, SyntaxTypeKind::Class)
        && !(namespace == "System" && &*declaration.name == "Object")
    {
        info.bases.push(named_symbol("System", "Object"));
    }
    let is_interface = matches!(declaration.kind, SyntaxTypeKind::Interface);
    let access = |modifiers: &[Modifier]| {
        if is_interface {
            Accessibility::Public
        } else {
            accessibility_of(modifiers)
        }
    };
    let mut prior_consts: BTreeMap<Box<str>, Literal> = BTreeMap::new();
    for member in &declaration.members {
        match member {
            Member::Field {
                modifiers,
                ty,
                declarators,
                ..
            } => {
                let field_ty = bind_type(ty);
                let is_const = modifiers.iter().any(|m| matches!(m, Modifier::Const));
                let is_static = is_static(modifiers) || is_const;
                let accessibility = access(modifiers);
                for declarator in declarators {
                    let constant = if is_const {
                        declarator
                            .initializer
                            .as_ref()
                            .and_then(|init| const_field_literal(init, &prior_consts))
                    } else {
                        None
                    };
                    if let Some(literal) = &constant {
                        prior_consts.insert(declarator.name.clone(), literal.clone());
                    }
                    info.fields.push(FieldSymbol {
                        name: declarator.name.clone(),
                        ty: field_ty.clone(),
                        is_static,
                        is_readonly: modifiers.iter().any(|m| matches!(m, Modifier::Readonly)),
                        is_volatile: modifiers.iter().any(|m| matches!(m, Modifier::Volatile)),
                        accessibility,
                        constant,
                    });
                }
            }
            Member::EventField {
                modifiers,
                ty,
                declarators,
                ..
            } => {
                let field_ty = bind_type(ty);
                let is_static = is_static(modifiers);
                let accessibility = access(modifiers);
                for declarator in declarators {
                    info.fields.push(FieldSymbol {
                        name: declarator.name.clone(),
                        ty: field_ty.clone(),
                        is_static,
                        is_readonly: false,
                        is_volatile: false,
                        accessibility,
                        constant: None,
                    });
                    info.events.push(EventSymbol {
                        name: declarator.name.clone(),
                        ty: field_ty.clone(),
                        is_static,
                        accessibility,
                        is_abstract: is_abstract_member(modifiers, info.kind),
                    });
                }
            }
            Member::Event {
                modifiers,
                ty,
                name,
                explicit_interface: None,
                ..
            } => info.events.push(EventSymbol {
                name: name.clone(),
                ty: bind_type(ty),
                is_static: is_static(modifiers),
                accessibility: access(modifiers),
                is_abstract: is_abstract_member(modifiers, info.kind),
            }),
            Member::Method {
                modifiers,
                return_type,
                name,
                parameters,
                is_vararg,
                explicit_interface,
                attributes,
                ..
            } => info.methods.push(MethodSymbol {
                name: match explicit_interface {
                    Some(interface) => explicit_interface_member_name(interface, name).into(),
                    None => name.clone(),
                },
                return_type: bind_type(return_type),
                parameters: parameters.iter().map(parameter_symbol).collect(),
                parameter_info: crate::bind::parameter_infos(parameters),
                is_static: explicit_interface.is_none() && is_static(modifiers),
                is_params: has_params_array(parameters),
                is_vararg: *is_vararg,
                is_virtual: is_virtual(modifiers),
                is_abstract: is_abstract_member(modifiers, info.kind),
                is_override: is_override(modifiers),
                is_sealed: is_sealed_member(modifiers),
                accessibility: match explicit_interface {
                    Some(_) => Accessibility::Private,
                    None => access(modifiers),
                },
                conditional: conditional_symbols_from_attributes(attributes),
            }),
            Member::Operator {
                return_type,
                operator,
                parameters,
                ..
            } => info.methods.push(MethodSymbol {
                name: operator.method_name(parameters.len()).into(),
                return_type: bind_type(return_type),
                parameters: parameters.iter().map(parameter_symbol).collect(),
                parameter_info: crate::bind::parameter_infos(parameters),
                is_static: true,
                is_params: false,
                is_vararg: false,
                is_virtual: false,
                is_abstract: false,
                is_override: false,
                is_sealed: false,
                accessibility: Accessibility::Public,
                conditional: Vec::new(),
            }),
            Member::ConversionOperator {
                direction,
                target,
                parameters,
                ..
            } => info.methods.push(MethodSymbol {
                name: direction.method_name().into(),
                return_type: bind_type(target),
                parameters: parameters.iter().map(parameter_symbol).collect(),
                parameter_info: crate::bind::parameter_infos(parameters),
                is_static: true,
                is_params: false,
                is_vararg: false,
                is_virtual: false,
                is_abstract: false,
                is_override: false,
                is_sealed: false,
                accessibility: Accessibility::Public,
                conditional: Vec::new(),
            }),
            Member::Property {
                modifiers,
                ty,
                name,
                getter,
                setter,
                ..
            } => info.properties.push(PropertySymbol {
                name: name.clone(),
                ty: bind_type(ty),
                is_static: is_static(modifiers),
                accessibility: access(modifiers),
                is_virtual: is_virtual(modifiers),
                is_abstract: is_abstract_member(modifiers, info.kind),
                is_override: is_override(modifiers),
                is_sealed: is_sealed_member(modifiers),
                has_getter: getter.is_some(),
                has_setter: setter.is_some(),
            }),
            Member::Indexer {
                modifiers,
                ty,
                parameters,
                getter,
                setter,
                ..
            } => {
                let element = bind_type(ty);
                let indices: Vec<TypeSymbol> = parameters.iter().map(parameter_symbol).collect();
                let accessibility = access(modifiers);
                let indexer_is_virtual = is_virtual(modifiers);
                let indexer_is_abstract = is_abstract_member(modifiers, info.kind);
                let indexer_is_override = is_override(modifiers);
                let indexer_is_sealed = is_sealed_member(modifiers);
                if getter.is_some() {
                    info.methods.push(MethodSymbol {
                        name: "get_Item".into(),
                        return_type: element.clone(),
                        parameters: indices.clone(),
                        parameter_info: crate::bind::parameter_infos(parameters),
                        is_static: false,
                        is_params: has_params_array(parameters),
                        is_vararg: false,
                        is_virtual: indexer_is_virtual,
                        is_abstract: indexer_is_abstract,
                        is_override: indexer_is_override,
                        is_sealed: indexer_is_sealed,
                        accessibility,
                        conditional: Vec::new(),
                    });
                }
                if setter.is_some() {
                    let mut info_with_value = crate::bind::parameter_infos(parameters);
                    info_with_value.push(crate::symbols::ParameterInfo {
                        name: "value".into(),
                        mode: crate::symbols::ParameterMode::Value,
                    });
                    let mut parameters = indices;
                    parameters.push(element);
                    info.methods.push(MethodSymbol {
                        name: "set_Item".into(),
                        return_type: TypeSymbol::Special(SpecialType::Void),
                        parameters,
                        parameter_info: info_with_value,
                        is_static: false,
                        is_params: false,
                        is_vararg: false,
                        is_virtual: indexer_is_virtual,
                        is_abstract: indexer_is_abstract,
                        is_override: indexer_is_override,
                        is_sealed: indexer_is_sealed,
                        accessibility,
                        conditional: Vec::new(),
                    });
                }
            }
            Member::Constructor {
                modifiers,
                parameters,
                is_vararg,
                ..
            } if !is_static(modifiers) => {
                let mut ctor = constructor(parameters, access(modifiers));
                ctor.is_vararg = *is_vararg;
                info.constructors.push(ctor)
            }
            _ => {}
        }
    }
    let has_parameterless = info.constructors.iter().any(|c| c.parameters.is_empty());
    match info.kind {
        TypeKind::Struct if !has_parameterless => {
            info.constructors.push(constructor(&[], Accessibility::Public))
        }
        TypeKind::Class
            if info.constructors.is_empty()
                && !declaration.modifiers.contains(&Modifier::Static) =>
        {
            info.constructors.push(constructor(&[], Accessibility::Public))
        }
        _ => {}
    }
    info
}

/// A constructor symbol from its parameters and declared accessibility. The return type is
/// unused (a `new` expression takes the created type), so it is left as `void`.
fn constructor(
    parameters: &[lamella_syntax::ast::Parameter],
    accessibility: Accessibility,
) -> MethodSymbol {
    MethodSymbol {
        name: ".ctor".into(),
        return_type: TypeSymbol::Special(SpecialType::Void),
        parameters: parameters.iter().map(parameter_symbol).collect(),
        parameter_info: crate::bind::parameter_infos(parameters),
        is_static: false,
        is_params: has_params_array(parameters),
        is_vararg: false,
        is_virtual: false,
        is_abstract: false,
        is_override: false,
        is_sealed: false,
        accessibility,
        conditional: Vec::new(),
    }
}

/// The `[Conditional("X")]` symbols declared on a source member (24.4.2): the attribute name is
/// matched as written (`Conditional` or the `Attribute`-suffixed form) and the symbol is its
/// first positional string-literal argument. A call to a source method marked this way is
/// omitted unless `X` is defined at the call site, like a BCL `Debug`/`Trace` method.
fn conditional_symbols_from_attributes(sections: &[AttributeSection]) -> Vec<Box<str>> {
    let mut symbols = Vec::new();
    for section in sections {
        if section.target.is_some() {
            continue;
        }
        for attribute in &section.attributes {
            let last = attribute.name.parts.last().map(|part| &**part);
            if last != Some("Conditional") && last != Some("ConditionalAttribute") {
                continue;
            }
            if let Some(AttributeArgument::Positional(expr)) = attribute.arguments.first() {
                if let ExprKind::Literal(Literal::String(units)) = &expr.kind {
                    if let Ok(symbol) = String::from_utf16(units) {
                        symbols.push(symbol.into_boxed_str());
                    }
                }
            }
        }
    }
    symbols
}

/// Whether a parameter list ends in a `params` array.
fn has_params_array(parameters: &[lamella_syntax::ast::Parameter]) -> bool {
    parameters.last().is_some_and(|parameter| {
        parameter.modifier == Some(lamella_syntax::ast::ParameterModifier::Params)
    })
}

/// A named-type symbol from a namespace and simple name, e.g. `"A.B"` + `Color`
/// gives `A.B.Color`; a `System` built-in folds to its special form.
fn named_symbol(namespace: &str, name: &str) -> TypeSymbol {
    let mut parts: alloc::vec::Vec<alloc::boxed::Box<str>> = alloc::vec::Vec::new();
    if !namespace.is_empty() {
        for part in namespace.split('.') {
            parts.push(part.into());
        }
    }
    parts.push(name.into());
    TypeSymbol::Named(parts.into_boxed_slice()).fold_builtin()
}

fn map_kind(kind: SyntaxTypeKind) -> TypeKind {
    match kind {
        SyntaxTypeKind::Class => TypeKind::Class,
        SyntaxTypeKind::Struct => TypeKind::Struct,
        SyntaxTypeKind::Interface => TypeKind::Interface,
    }
}

fn is_static(modifiers: &[Modifier]) -> bool {
    modifiers.contains(&Modifier::Static)
}

fn is_virtual(modifiers: &[Modifier]) -> bool {
    modifiers.contains(&Modifier::Virtual)
}

fn is_override(modifiers: &[Modifier]) -> bool {
    modifiers.contains(&Modifier::Override)
}

/// Whether a member is declared `sealed`. On a member this only ever accompanies `override`
/// (17.5.5): it CLOSES the slot, so no further derived class may override it.
fn is_sealed_member(modifiers: &[Modifier]) -> bool {
    modifiers.contains(&Modifier::Sealed)
}

/// Whether a member declared with `modifiers` in a type of `kind` is abstract: an `abstract`
/// modifier, or -- implicitly -- any member of an interface (17.2.2 / 20.2).
fn is_abstract_member(modifiers: &[Modifier], kind: TypeKind) -> bool {
    modifiers.contains(&Modifier::Abstract) || kind == TypeKind::Interface
}

/// The accessibility a member's modifiers declare; a class member with none is
/// `private` (10.5.1).
pub(crate) fn accessibility_of(modifiers: &[Modifier]) -> Accessibility {
    let protected = modifiers.contains(&Modifier::Protected);
    let internal = modifiers.contains(&Modifier::Internal);
    if modifiers.contains(&Modifier::Public) {
        Accessibility::Public
    } else if protected && internal {
        Accessibility::ProtectedInternal
    } else if protected {
        Accessibility::Protected
    } else if internal {
        Accessibility::Internal
    } else {
        Accessibility::Private
    }
}

/// Appends a (possibly dotted) namespace declaration name to the enclosing
/// namespace, e.g. `"A"` and `B.C` give `"A.B.C"`.
fn join_namespace(outer: &str, name: &QualifiedName) -> String {
    let mut joined = String::from(outer);
    for part in &name.parts {
        if !joined.is_empty() {
            joined.push('.');
        }
        joined.push_str(part);
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use lamella_syntax::parser::parse_compilation_unit;

    /// DIAGNOSTIC probe for the const-of-const mis-fold (@layering, 2026-07-28): does the second
    /// pass actually fold these four shapes into the model, or is the loss downstream of it?
    #[test]
    fn resolve_constants_folds_a_const_that_names_another_const() {
        let unit = parse_compilation_unit(
            "namespace G { public sealed class Facts { \
                 public const uint IDENTITY_REG = 0xD0; \
                 public const uint DIG_P9_WIDTH = 16; \
                 public const uint OSRS_T_LSB = 5; } } \
             namespace P { using G; public sealed class C { \
                 private const byte CROSS = (byte)Facts.IDENTITY_REG; \
                 private const int ARITH = (int)(Facts.DIG_P9_WIDTH + Facts.OSRS_T_LSB + 3); \
                 private const int LOCAL_SRC = 208; \
                 private const int LOCAL = LOCAL_SRC; } }",
        )
        .unit;
        let mut model = Model::new();
        collect_into(&mut model, &unit);
        model.canonicalize_signatures();
        model.link_bases();
        resolve_constants(&mut model, core::slice::from_ref(&unit));

        let constant = |ty: &str, field: &str| {
            model
                .get("P", ty)
                .and_then(|info| {
                    info.fields
                        .iter()
                        .find(|f| &*f.name == field)
                        .map(|f| f.constant.clone())
                })
                .expect("the type and field exist")
        };
        assert!(constant("C", "CROSS").is_some(), "cast of a cross-class const");
        assert!(constant("C", "ARITH").is_some(), "arithmetic over cross-class consts");
        assert!(constant("C", "LOCAL").is_some(), "a const naming a same-class const");
    }

    #[test]
    fn an_enum_member_takes_its_value_from_a_cross_class_const() {
        let unit = parse_compilation_unit(
            "namespace G { public sealed class Facts { \
                 public const uint MODE_NORMAL = 3; \
                 public const uint MODE_FORCED = 1; } } \
             namespace P { using G; \
                 public enum Mode { Normal = (int)Facts.MODE_NORMAL, Next, \
                                    Forced = (int)Facts.MODE_FORCED } }",
        )
        .unit;
        let mut model = Model::new();
        collect_into(&mut model, &unit);
        model.canonicalize_signatures();
        model.link_bases();
        resolve_constants(&mut model, core::slice::from_ref(&unit));

        let value = |member: &str| {
            model
                .get("P", "Mode")
                .and_then(|info| info.fields.iter().find(|f| &*f.name == member).cloned())
                .and_then(|f| f.constant)
                .and_then(|literal| literal_int_value(&literal))
                .expect("the member exists and has a constant")
        };
        assert_eq!(value("Normal"), 3, "folded from the cross-class const");
        assert_eq!(value("Next"), 4, "auto-numbered from the resolved member");
        assert_eq!(value("Forced"), 1, "a later explicit member re-anchors");
    }

    #[test]
    fn collects_top_level_namespaced_and_nested_namespace_types() {
        let unit = parse_compilation_unit(
            "class Bar {} enum E { A } \
             namespace A.B { class Foo {} delegate void D(); \
                namespace C { struct S {} } }",
        )
        .unit;
        let table = collect_types(&unit);
        assert!(table.contains("", "Bar"));
        assert!(table.contains("", "E"));
        assert!(table.contains("A.B", "Foo"));
        assert!(table.contains("A.B", "D"));
        assert!(table.contains("A.B.C", "S"));
        assert!(!table.contains("", "Foo"));
        assert!(!table.contains("", "Missing"));
    }

    #[test]
    fn collects_fields_and_methods_of_a_source_type() {
        let unit = parse_compilation_unit(
            "namespace N { class Widget { \
                int count; \
                static int Make(int n, string s) { } \
                double Area() { } \
             } }",
        )
        .unit;
        let model = collect_model(&unit);
        let widget = model
            .get("N", "Widget")
            .expect("Widget should be collected");
        assert_eq!(widget.kind, TypeKind::Class);
        assert_eq!(
            widget.find_field("count").map(|field| field.ty.to_string()),
            Some("int".to_string())
        );
        let make = widget.methods_named("Make").next().expect("Make");
        assert!(make.is_static);
        assert_eq!(make.parameters.len(), 2);
        assert_eq!(make.return_type.to_string(), "int");
        let area = widget.methods_named("Area").next().expect("Area");
        assert!(!area.is_static);
        assert!(area.return_type.to_string() == "double");
    }
}
