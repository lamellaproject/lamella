//! Rebuilding an owned `ast::Program` from an artifact.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::ast::*;
use crate::string_value::JsString;
use crate::source::Span;

use lamella_js_bytecode::format::{FormatError, Tag, FN_ASYNC, FN_GENERATOR, FN_STRICT};
use lamella_js_bytecode::read::{Artifact, Fields, Node};

/// Rebuilds the program an artifact holds.
pub fn decode(artifact: &Artifact<'_>) -> Result<Program, FormatError> {
    let root = artifact.root()?;
    root.expect(Tag::Program, "Program")?;
    let mut fields = root.fields();
    let span = span(&mut fields)?;
    let strict = fields.bool()?;
    let body = statements(artifact, &mut fields)?;
    fields.finish(root.offset())?;
    Ok(Program { body, strict, span })
}

fn span(fields: &mut Fields<'_>) -> Result<Span, FormatError> {
    let raw = fields.span()?;
    Ok(Span { start: raw.start as usize, end: raw.end as usize })
}

fn text(artifact: &Artifact<'_>, fields: &mut Fields<'_>) -> Result<String, FormatError> {
    let id = fields.str_id()?;
    Ok(String::from(artifact.str_utf8(id)?))
}

fn opt_text(artifact: &Artifact<'_>, fields: &mut Fields<'_>) -> Result<Option<String>, FormatError> {
    if fields.bool()? {
        Ok(Some(text(artifact, fields)?))
    } else {
        Ok(None)
    }
}

/// Rebuilt from code UNITS, so a lone surrogate survives. `JsString::from_units` is the only
/// constructor that can express one; going through `str` would turn `'\uD834'` into a replacement
/// character and quietly implement a different language.
fn js_text(artifact: &Artifact<'_>, fields: &mut Fields<'_>) -> Result<JsString, FormatError> {
    let id = fields.str_id()?;
    let units: Vec<u16> = artifact.str_utf16(id)?.collect();
    Ok(JsString::from_units(&units))
}

fn statements(artifact: &Artifact<'_>, fields: &mut Fields<'_>) -> Result<Vec<Statement>, FormatError> {
    let count = fields.count()? as usize;
    let mut body = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        body.push(statement(artifact, &fields.child()?)?);
    }
    Ok(body)
}

pub fn statement(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<Statement, FormatError> {
    let mut f = node.fields();
    let at = node.offset();
    let statement = match node.tag() {
        Tag::StmtExpression => {
            let span = span(&mut f)?;
            Statement::Expression { expression: expression(artifact, &f.child()?)?, span }
        }
        Tag::StmtBlock => {
            let span = span(&mut f)?;
            Statement::Block { body: statements(artifact, &mut f)?, span }
        }
        Tag::StmtEmpty => Statement::Empty { span: span(&mut f)? },
        Tag::StmtDeclaration => {
            let span = span(&mut f)?;
            let kind = declaration_kind(f.byte()?, at)?;
            Statement::Declaration { kind, declarations: declarators(artifact, &mut f)?, span }
        }
        Tag::StmtIf => {
            let span = span(&mut f)?;
            let test = expression(artifact, &f.child()?)?;
            let consequent = Box::new(statement(artifact, &f.child()?)?);
            let alternate = match f.option_child()? {
                Some(child) => Some(Box::new(statement(artifact, &child)?)),
                None => None,
            };
            Statement::If { test, consequent, alternate, span }
        }
        Tag::StmtWhile => {
            let span = span(&mut f)?;
            let test = expression(artifact, &f.child()?)?;
            let body = Box::new(statement(artifact, &f.child()?)?);
            Statement::While { test, body, span }
        }
        Tag::StmtWith => {
            let span = span(&mut f)?;
            let object = expression(artifact, &f.child()?)?;
            let body = Box::new(statement(artifact, &f.child()?)?);
            Statement::With { object, body, span }
        }
        Tag::StmtDoWhile => {
            let span = span(&mut f)?;
            let body = Box::new(statement(artifact, &f.child()?)?);
            let test = expression(artifact, &f.child()?)?;
            Statement::DoWhile { body, test, span }
        }
        Tag::StmtFor => {
            let span = span(&mut f)?;
            let init = match f.option_child()? {
                Some(child) => Some(Box::new(for_init(artifact, &child)?)),
                None => None,
            };
            let test = opt_expression(artifact, &mut f)?;
            let update = opt_expression(artifact, &mut f)?;
            let body = Box::new(statement(artifact, &f.child()?)?);
            Statement::For { init, test, update, body, span }
        }
        Tag::StmtForIn => {
            let span = span(&mut f)?;
            let left = Box::new(for_init(artifact, &f.child()?)?);
            let right = expression(artifact, &f.child()?)?;
            let body = Box::new(statement(artifact, &f.child()?)?);
            Statement::ForIn { left, right, body, span }
        }
        Tag::StmtForOf => {
            let span = span(&mut f)?;
            let left = Box::new(for_init(artifact, &f.child()?)?);
            let right = expression(artifact, &f.child()?)?;
            let body = Box::new(statement(artifact, &f.child()?)?);
            Statement::ForOf { left, right, body, span }
        }
        Tag::StmtReturn => {
            let span = span(&mut f)?;
            Statement::Return { argument: opt_expression(artifact, &mut f)?, span }
        }
        Tag::StmtBreak => {
            let span = span(&mut f)?;
            Statement::Break { label: opt_text(artifact, &mut f)?, span }
        }
        Tag::StmtContinue => {
            let span = span(&mut f)?;
            Statement::Continue { label: opt_text(artifact, &mut f)?, span }
        }
        Tag::StmtThrow => {
            let span = span(&mut f)?;
            Statement::Throw { argument: expression(artifact, &f.child()?)?, span }
        }
        Tag::StmtTry => {
            let span = span(&mut f)?;
            let block = statements(artifact, &mut f)?;
            let handler = match f.option_child()? {
                Some(child) => Some(catch_clause(artifact, &child)?),
                None => None,
            };
            let finalizer =
                if f.bool()? { Some(statements(artifact, &mut f)?) } else { None };
            Statement::Try { block, handler, finalizer, span }
        }
        Tag::StmtLabeled => {
            let span = span(&mut f)?;
            let label = text(artifact, &mut f)?;
            let body = Box::new(statement(artifact, &f.child()?)?);
            Statement::Labeled { label, body, span }
        }
        Tag::StmtSwitch => {
            let span = span(&mut f)?;
            let discriminant = expression(artifact, &f.child()?)?;
            let count = f.count()? as usize;
            let mut cases = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                cases.push(switch_case(artifact, &f.child()?)?);
            }
            Statement::Switch { discriminant, cases, span }
        }
        Tag::StmtFunction => Statement::Function(Box::new(function(artifact, &f.child()?)?)),
        Tag::StmtClass => Statement::Class(Box::new(class(artifact, &f.child()?)?)),
        Tag::StmtDebugger => Statement::Debugger { span: span(&mut f)? },
        other => {
            return Err(FormatError::UnexpectedTag { at, tag: other as u8, expected: "Statement" })
        }
    };
    f.finish(at)?;
    Ok(statement)
}

fn opt_expression(
    artifact: &Artifact<'_>,
    fields: &mut Fields<'_>,
) -> Result<Option<Expression>, FormatError> {
    match fields.option_child()? {
        Some(child) => Ok(Some(expression(artifact, &child)?)),
        None => Ok(None),
    }
}

pub fn expression(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<Expression, FormatError> {
    let mut f = node.fields();
    let at = node.offset();
    let expression = match node.tag() {
        Tag::ExprIdentifier => {
            let span = span(&mut f)?;
            Expression::Identifier { name: text(artifact, &mut f)?, span }
        }
        Tag::ExprNumberSmall => {
            let span = span(&mut f)?;
            #[allow(clippy::cast_precision_loss)]
            Expression::Number { value: f.i64()? as f64, span }
        }
        Tag::ExprNumber => {
            let span = span(&mut f)?;
            Expression::Number { value: f64::from_bits(f.f64_bits()?), span }
        }
        Tag::ExprString => {
            let span = span(&mut f)?;
            Expression::String { value: js_text(artifact, &mut f)?, span }
        }
        Tag::ExprBoolean => {
            let span = span(&mut f)?;
            Expression::Boolean { value: f.bool()?, span }
        }
        Tag::ExprNull => Expression::Null { span: span(&mut f)? },
        Tag::ExprThis => Expression::This { span: span(&mut f)? },
        Tag::ExprTemplate => {
            let span = span(&mut f)?;
            let count = f.count()? as usize;
            let mut quasis = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                quasis.push(template_element(artifact, &f.child()?)?);
            }
            let count = f.count()? as usize;
            let mut expressions = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                expressions.push(expression(artifact, &f.child()?)?);
            }
            Expression::Template { quasis, expressions, span }
        }
        Tag::ExprTagged => {
            let span = span(&mut f)?;
            let tag = Box::new(expression(artifact, &f.child()?)?);
            let quasi = Box::new(expression(artifact, &f.child()?)?);
            Expression::Tagged { tag, quasi, span }
        }
        Tag::ExprRegExp => {
            let span = span(&mut f)?;
            let body = text(artifact, &mut f)?;
            let flags = text(artifact, &mut f)?;
            Expression::RegExp { body, flags, span }
        }
        Tag::ExprArray => {
            let span = span(&mut f)?;
            let count = f.count()? as usize;
            let mut elements = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                elements.push(array_element(artifact, &f.child()?)?);
            }
            Expression::Array { elements, span }
        }
        Tag::ExprObject => {
            let span = span(&mut f)?;
            let count = f.count()? as usize;
            let mut properties = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                properties.push(object_property(artifact, &f.child()?)?);
            }
            Expression::Object { properties, span }
        }
        Tag::ExprFunction => Expression::Function(Box::new(function(artifact, &f.child()?)?)),
        Tag::ExprArrow => Expression::Arrow(Box::new(arrow(artifact, &f.child()?)?)),
        Tag::ExprClass => Expression::Class(Box::new(class(artifact, &f.child()?)?)),
        Tag::ExprSuper => Expression::Super { span: span(&mut f)? },
        Tag::ExprNewTarget => Expression::NewTarget { span: span(&mut f)? },
        Tag::ExprUnary => {
            let span = span(&mut f)?;
            let operator = unary_operator(f.byte()?, at)?;
            let argument = Box::new(expression(artifact, &f.child()?)?);
            Expression::Unary { operator, argument, span }
        }
        Tag::ExprUpdate => {
            let span = span(&mut f)?;
            let operator = update_operator(f.byte()?, at)?;
            let prefix = f.bool()?;
            let argument = Box::new(expression(artifact, &f.child()?)?);
            Expression::Update { operator, prefix, argument, span }
        }
        Tag::ExprBinary => {
            let span = span(&mut f)?;
            let operator = binary_operator(f.byte()?, at)?;
            let left = Box::new(expression(artifact, &f.child()?)?);
            let right = Box::new(expression(artifact, &f.child()?)?);
            Expression::Binary { operator, left, right, span }
        }
        Tag::ExprLogical => {
            let span = span(&mut f)?;
            let operator = logical_operator(f.byte()?, at)?;
            let left = Box::new(expression(artifact, &f.child()?)?);
            let right = Box::new(expression(artifact, &f.child()?)?);
            Expression::Logical { operator, left, right, span }
        }
        Tag::ExprAssignment => {
            let span = span(&mut f)?;
            let operator = assignment_operator(f.byte()?, at)?;
            let target = Box::new(assignment_target(artifact, &f.child()?)?);
            let value = Box::new(expression(artifact, &f.child()?)?);
            Expression::Assignment { operator, target, value, span }
        }
        Tag::ExprConditional => {
            let span = span(&mut f)?;
            let test = Box::new(expression(artifact, &f.child()?)?);
            let consequent = Box::new(expression(artifact, &f.child()?)?);
            let alternate = Box::new(expression(artifact, &f.child()?)?);
            Expression::Conditional { test, consequent, alternate, span }
        }
        Tag::ExprCall => {
            let span = span(&mut f)?;
            let callee = Box::new(expression(artifact, &f.child()?)?);
            let optional = f.bool()?;
            let arguments = arguments(artifact, &mut f)?;
            Expression::Call { callee, arguments, optional, span }
        }
        Tag::ExprNew => {
            let span = span(&mut f)?;
            let callee = Box::new(expression(artifact, &f.child()?)?);
            let arguments = arguments(artifact, &mut f)?;
            Expression::New { callee, arguments, span }
        }
        Tag::ExprMember => {
            let span = span(&mut f)?;
            let object = Box::new(expression(artifact, &f.child()?)?);
            let property = Box::new(member_property(artifact, &f.child()?)?);
            let optional = f.bool()?;
            Expression::Member { object, property, optional, span }
        }
        Tag::ExprSequence => {
            let span = span(&mut f)?;
            let count = f.count()? as usize;
            let mut expressions = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                expressions.push(expression(artifact, &f.child()?)?);
            }
            Expression::Sequence { expressions, span }
        }
        Tag::ExprParenthesized => {
            let span = span(&mut f)?;
            let expression = Box::new(expression(artifact, &f.child()?)?);
            Expression::Parenthesized { expression, span }
        }
        other => {
            return Err(FormatError::UnexpectedTag { at, tag: other as u8, expected: "Expression" })
        }
    };
    f.finish(at)?;
    Ok(expression)
}

fn arguments(artifact: &Artifact<'_>, fields: &mut Fields<'_>) -> Result<Vec<Argument>, FormatError> {
    let count = fields.count()? as usize;
    let mut arguments = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let node = fields.child()?;
        let mut f = node.fields();
        let at = node.offset();
        let argument = match node.tag() {
            Tag::ArgExpression => Argument::Expression(expression(artifact, &f.child()?)?),
            Tag::ArgSpread => {
                let span = span(&mut f)?;
                Argument::Spread { argument: expression(artifact, &f.child()?)?, span }
            }
            other => {
                return Err(FormatError::UnexpectedTag { at, tag: other as u8, expected: "Argument" })
            }
        };
        f.finish(at)?;
        arguments.push(argument);
    }
    Ok(arguments)
}

fn array_element(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<ArrayElement, FormatError> {
    let mut f = node.fields();
    let at = node.offset();
    let element = match node.tag() {
        Tag::ElemHole => ArrayElement::Hole,
        Tag::ElemExpression => ArrayElement::Expression(expression(artifact, &f.child()?)?),
        Tag::ElemSpread => {
            let span = span(&mut f)?;
            let argument = expression(artifact, &f.child()?)?;
            let comma_after = f.bool()?;
            ArrayElement::Spread { argument, span, comma_after }
        }
        other => {
            return Err(FormatError::UnexpectedTag { at, tag: other as u8, expected: "ArrayElement" })
        }
    };
    f.finish(at)?;
    Ok(element)
}

fn object_property(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<ObjectProperty, FormatError> {
    let mut f = node.fields();
    let at = node.offset();
    let property = match node.tag() {
        Tag::ObjProperty => {
            let span = span(&mut f)?;
            let key = property_key(artifact, &f.child()?)?;
            let value = expression(artifact, &f.child()?)?;
            let computed = f.bool()?;
            let shorthand = f.bool()?;
            ObjectProperty::Property { key, value, computed, shorthand, span }
        }
        Tag::ObjCoverInitializedName => {
            let span = span(&mut f)?;
            let name = text(artifact, &mut f)?;
            let value = expression(artifact, &f.child()?)?;
            ObjectProperty::CoverInitializedName { name, value, span }
        }
        Tag::ObjMethod => {
            let span = span(&mut f)?;
            let key = property_key(artifact, &f.child()?)?;
            let function = Box::new(function(artifact, &f.child()?)?);
            let kind = method_kind(f.byte()?, at)?;
            let computed = f.bool()?;
            ObjectProperty::Method { key, function, kind, computed, span }
        }
        Tag::ObjSpread => {
            let span = span(&mut f)?;
            ObjectProperty::Spread { argument: expression(artifact, &f.child()?)?, span }
        }
        other => {
            return Err(FormatError::UnexpectedTag {
                at,
                tag: other as u8,
                expected: "ObjectProperty",
            })
        }
    };
    f.finish(at)?;
    Ok(property)
}

pub fn property_key(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<PropertyKey, FormatError> {
    let mut f = node.fields();
    let at = node.offset();
    let key = match node.tag() {
        Tag::KeyIdentifier => {
            let span = span(&mut f)?;
            PropertyKey::Identifier { name: text(artifact, &mut f)?, span }
        }
        Tag::KeyString => {
            let span = span(&mut f)?;
            PropertyKey::String { value: js_text(artifact, &mut f)?, span }
        }
        Tag::KeyNumber => {
            let span = span(&mut f)?;
            PropertyKey::Number { value: f64::from_bits(f.f64_bits()?), span }
        }
        Tag::KeyComputed => {
            let span = span(&mut f)?;
            let expression = Box::new(expression(artifact, &f.child()?)?);
            PropertyKey::Computed { expression, span }
        }
        Tag::KeyPrivate => {
            let span = span(&mut f)?;
            PropertyKey::Private { name: text(artifact, &mut f)?, span }
        }
        other => {
            return Err(FormatError::UnexpectedTag { at, tag: other as u8, expected: "PropertyKey" })
        }
    };
    f.finish(at)?;
    Ok(key)
}

fn member_property(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<MemberProperty, FormatError> {
    let mut f = node.fields();
    let at = node.offset();
    let property = match node.tag() {
        Tag::MemberIdentifier => {
            let span = span(&mut f)?;
            MemberProperty::Identifier { name: text(artifact, &mut f)?, span }
        }
        Tag::MemberComputed => {
            let span = span(&mut f)?;
            MemberProperty::Computed { expression: expression(artifact, &f.child()?)?, span }
        }
        Tag::MemberPrivate => {
            let span = span(&mut f)?;
            MemberProperty::Private { name: text(artifact, &mut f)?, span }
        }
        other => {
            return Err(FormatError::UnexpectedTag {
                at,
                tag: other as u8,
                expected: "MemberProperty",
            })
        }
    };
    f.finish(at)?;
    Ok(property)
}

/// The string pieces of a template literal node, without its interpolations.
///
/// Exposed for the walker's TAGGED template arm, which needs the pieces -- cooked AND raw -- to
/// build the frozen `strings` object a tag receives. That object is cached per call site, so this
/// runs once per site rather than once per evaluation, which is what keeps decoding acceptable on a
/// path whose whole purpose is not to rebuild the tree.
pub fn template_elements(
    artifact: &Artifact<'_>,
    node: &Node<'_>,
) -> Result<Vec<TemplateElement>, FormatError> {
    node.expect(Tag::ExprTemplate, "ExprTemplate")?;
    let mut f = node.fields();
    span(&mut f)?;
    let count = f.count()? as usize;
    let mut quasis = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        quasis.push(template_element(artifact, &f.child()?)?);
    }
    Ok(quasis)
}

fn template_element(
    artifact: &Artifact<'_>,
    node: &Node<'_>,
) -> Result<TemplateElement, FormatError> {
    node.expect(Tag::TemplateElement, "TemplateElement")?;
    let mut f = node.fields();
    let at = node.offset();
    let span = span(&mut f)?;
    let cooked = if f.bool()? { Some(js_text(artifact, &mut f)?) } else { None };
    let raw = text(artifact, &mut f)?;
    f.finish(at)?;
    Ok(TemplateElement { cooked, raw, span })
}

pub fn for_init(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<ForInit, FormatError> {
    let mut f = node.fields();
    let at = node.offset();
    let init = match node.tag() {
        Tag::ForInitDeclaration => {
            let span = span(&mut f)?;
            let kind = declaration_kind(f.byte()?, at)?;
            ForInit::Declaration { kind, declarations: declarators(artifact, &mut f)?, span }
        }
        Tag::ForInitExpression => ForInit::Expression(expression(artifact, &f.child()?)?),
        Tag::ForInitPattern => ForInit::Pattern(pattern(artifact, &f.child()?)?),
        other => {
            return Err(FormatError::UnexpectedTag { at, tag: other as u8, expected: "ForInit" })
        }
    };
    f.finish(at)?;
    Ok(init)
}

fn declarators(
    artifact: &Artifact<'_>,
    fields: &mut Fields<'_>,
) -> Result<Vec<Declarator>, FormatError> {
    let count = fields.count()? as usize;
    let mut declarations = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let node = fields.child()?;
        node.expect(Tag::Declarator, "Declarator")?;
        let mut f = node.fields();
        let at = node.offset();
        let span = span(&mut f)?;
        let target = pattern(artifact, &f.child()?)?;
        let init = opt_expression(artifact, &mut f)?;
        f.finish(at)?;
        declarations.push(Declarator { target, init, span });
    }
    Ok(declarations)
}

fn catch_clause(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<CatchClause, FormatError> {
    node.expect(Tag::CatchClause, "CatchClause")?;
    let mut f = node.fields();
    let at = node.offset();
    let span = span(&mut f)?;
    let param = match f.option_child()? {
        Some(child) => Some(pattern(artifact, &child)?),
        None => None,
    };
    let body = statements(artifact, &mut f)?;
    f.finish(at)?;
    Ok(CatchClause { param, body, span })
}

fn switch_case(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<SwitchCase, FormatError> {
    node.expect(Tag::SwitchCase, "SwitchCase")?;
    let mut f = node.fields();
    let at = node.offset();
    let span = span(&mut f)?;
    let test = opt_expression(artifact, &mut f)?;
    let body = statements(artifact, &mut f)?;
    f.finish(at)?;
    Ok(SwitchCase { test, body, span })
}

pub fn pattern(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<Pattern, FormatError> {
    let mut f = node.fields();
    let at = node.offset();
    let pattern = match node.tag() {
        Tag::PatIdentifier => {
            let span = span(&mut f)?;
            Pattern::Identifier { name: text(artifact, &mut f)?, span }
        }
        Tag::PatMember => {
            let span = span(&mut f)?;
            let object = Box::new(expression(artifact, &f.child()?)?);
            let property = Box::new(member_property(artifact, &f.child()?)?);
            let optional = f.bool()?;
            Pattern::Member { object, property, optional, span }
        }
        Tag::PatArray => {
            let span = span(&mut f)?;
            let count = f.count()? as usize;
            let mut elements = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                match f.option_child()? {
                    Some(child) => elements.push(Some(pattern(artifact, &child)?)),
                    None => elements.push(None),
                }
            }
            let rest = opt_pattern(artifact, &mut f)?;
            Pattern::Array { elements, rest, span }
        }
        Tag::PatObject => {
            let span = span(&mut f)?;
            let count = f.count()? as usize;
            let mut properties = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                properties.push(object_pattern_property(artifact, &f.child()?)?);
            }
            let rest = opt_pattern(artifact, &mut f)?;
            Pattern::Object { properties, rest, span }
        }
        Tag::PatDefault => {
            let span = span(&mut f)?;
            let target = Box::new(pattern(artifact, &f.child()?)?);
            let value = Box::new(expression(artifact, &f.child()?)?);
            Pattern::Default { target, value, span }
        }
        Tag::PatRest => {
            let span = span(&mut f)?;
            let argument = Box::new(pattern(artifact, &f.child()?)?);
            Pattern::Rest { argument, span }
        }
        other => {
            return Err(FormatError::UnexpectedTag { at, tag: other as u8, expected: "Pattern" })
        }
    };
    f.finish(at)?;
    Ok(pattern)
}

fn opt_pattern(
    artifact: &Artifact<'_>,
    fields: &mut Fields<'_>,
) -> Result<Option<Box<Pattern>>, FormatError> {
    match fields.option_child()? {
        Some(child) => Ok(Some(Box::new(pattern(artifact, &child)?))),
        None => Ok(None),
    }
}

fn object_pattern_property(
    artifact: &Artifact<'_>,
    node: &Node<'_>,
) -> Result<ObjectPatternProperty, FormatError> {
    node.expect(Tag::ObjectPatternProperty, "ObjectPatternProperty")?;
    let mut f = node.fields();
    let at = node.offset();
    let span = span(&mut f)?;
    let key = property_key(artifact, &f.child()?)?;
    let value = pattern(artifact, &f.child()?)?;
    let computed = f.bool()?;
    let shorthand = f.bool()?;
    f.finish(at)?;
    Ok(ObjectPatternProperty { key, value, computed, shorthand, span })
}

fn assignment_target(
    artifact: &Artifact<'_>,
    node: &Node<'_>,
) -> Result<AssignmentTarget, FormatError> {
    let mut f = node.fields();
    let at = node.offset();
    let target = match node.tag() {
        Tag::TargetPattern => AssignmentTarget::Pattern {
            pattern: pattern(artifact, &f.child()?)?,
            parenthesized: f.bool()?,
        },
        Tag::TargetInvalid => AssignmentTarget::Invalid(expression(artifact, &f.child()?)?),
        other => {
            return Err(FormatError::UnexpectedTag {
                at,
                tag: other as u8,
                expected: "AssignmentTarget",
            })
        }
    };
    f.finish(at)?;
    Ok(target)
}

fn function(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<Function, FormatError> {
    node.expect(Tag::Function, "Function")?;
    let mut f = node.fields();
    let at = node.offset();
    let span = span(&mut f)?;
    let name = opt_text(artifact, &mut f)?;
    let count = f.count()? as usize;
    let mut params = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        params.push(pattern(artifact, &f.child()?)?);
    }
    let body = statements(artifact, &mut f)?;
    let flags = f.byte()?;
    f.finish(at)?;
    Ok(Function {
        name,
        params,
        body,
        is_generator: flags & FN_GENERATOR != 0,
        is_async: flags & FN_ASYNC != 0,
        strict: flags & FN_STRICT != 0,
        span,
    })
}

fn arrow(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<Arrow, FormatError> {
    node.expect(Tag::Arrow, "Arrow")?;
    let mut f = node.fields();
    let at = node.offset();
    let span = span(&mut f)?;
    let count = f.count()? as usize;
    let mut params = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        params.push(pattern(artifact, &f.child()?)?);
    }
    let body_node = f.child()?;
    let mut b = body_node.fields();
    let body_at = body_node.offset();
    let body = match body_node.tag() {
        Tag::ArrowBodyExpression => {
            ArrowBody::Expression(Box::new(expression(artifact, &b.child()?)?))
        }
        Tag::ArrowBodyBlock => ArrowBody::Block(statements(artifact, &mut b)?),
        other => {
            return Err(FormatError::UnexpectedTag {
                at: body_at,
                tag: other as u8,
                expected: "ArrowBody",
            })
        }
    };
    b.finish(body_at)?;
    let flags = f.byte()?;
    f.finish(at)?;
    Ok(Arrow {
        params,
        body,
        is_async: flags & FN_ASYNC != 0,
        strict: flags & FN_STRICT != 0,
        span,
    })
}

fn class(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<Class, FormatError> {
    node.expect(Tag::Class, "Class")?;
    let mut f = node.fields();
    let at = node.offset();
    let span = span(&mut f)?;
    let name = opt_text(artifact, &mut f)?;
    let heritage = match f.option_child()? {
        Some(child) => Some(Box::new(expression(artifact, &child)?)),
        None => None,
    };
    let count = f.count()? as usize;
    let mut members = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        members.push(class_member(artifact, &f.child()?)?);
    }
    f.finish(at)?;
    Ok(Class { name, heritage, members, span })
}

fn class_member(artifact: &Artifact<'_>, node: &Node<'_>) -> Result<ClassMember, FormatError> {
    node.expect(Tag::ClassMember, "ClassMember")?;
    let mut f = node.fields();
    let at = node.offset();
    let span = span(&mut f)?;
    let key = property_key(artifact, &f.child()?)?;
    let kind = method_kind(f.byte()?, at)?;
    let is_static = f.bool()?;
    let computed = f.bool()?;
    let function = Box::new(function(artifact, &f.child()?)?);
    f.finish(at)?;
    Ok(ClassMember { key, kind, is_static, computed, function, span })
}


pub fn declaration_kind(byte: u8, at: usize) -> Result<DeclarationKind, FormatError> {
    Ok(match byte {
        0 => DeclarationKind::Var,
        1 => DeclarationKind::Let,
        2 => DeclarationKind::Const,
        _ => return Err(FormatError::BadTag { at, tag: byte }),
    })
}

pub fn method_kind(byte: u8, at: usize) -> Result<MethodKind, FormatError> {
    Ok(match byte {
        0 => MethodKind::Normal,
        1 => MethodKind::Get,
        2 => MethodKind::Set,
        _ => return Err(FormatError::BadTag { at, tag: byte }),
    })
}

pub fn unary_operator(byte: u8, at: usize) -> Result<UnaryOperator, FormatError> {
    Ok(match byte {
        0 => UnaryOperator::Minus,
        1 => UnaryOperator::Plus,
        2 => UnaryOperator::Not,
        3 => UnaryOperator::BitwiseNot,
        4 => UnaryOperator::TypeOf,
        5 => UnaryOperator::Void,
        6 => UnaryOperator::Delete,
        7 => UnaryOperator::Await,
        _ => return Err(FormatError::BadTag { at, tag: byte }),
    })
}

pub fn update_operator(byte: u8, at: usize) -> Result<UpdateOperator, FormatError> {
    Ok(match byte {
        0 => UpdateOperator::Increment,
        1 => UpdateOperator::Decrement,
        _ => return Err(FormatError::BadTag { at, tag: byte }),
    })
}

pub fn binary_operator(byte: u8, at: usize) -> Result<BinaryOperator, FormatError> {
    Ok(match byte {
        0 => BinaryOperator::Add,
        1 => BinaryOperator::Subtract,
        2 => BinaryOperator::Multiply,
        3 => BinaryOperator::Divide,
        4 => BinaryOperator::Remainder,
        5 => BinaryOperator::Exponent,
        6 => BinaryOperator::Equal,
        7 => BinaryOperator::NotEqual,
        8 => BinaryOperator::StrictEqual,
        9 => BinaryOperator::StrictNotEqual,
        10 => BinaryOperator::Less,
        11 => BinaryOperator::LessEqual,
        12 => BinaryOperator::Greater,
        13 => BinaryOperator::GreaterEqual,
        14 => BinaryOperator::ShiftLeft,
        15 => BinaryOperator::ShiftRight,
        16 => BinaryOperator::UnsignedShiftRight,
        17 => BinaryOperator::BitwiseAnd,
        18 => BinaryOperator::BitwiseOr,
        19 => BinaryOperator::BitwiseXor,
        20 => BinaryOperator::In,
        21 => BinaryOperator::InstanceOf,
        _ => return Err(FormatError::BadTag { at, tag: byte }),
    })
}

pub fn logical_operator(byte: u8, at: usize) -> Result<LogicalOperator, FormatError> {
    Ok(match byte {
        0 => LogicalOperator::And,
        1 => LogicalOperator::Or,
        2 => LogicalOperator::NullishCoalescing,
        _ => return Err(FormatError::BadTag { at, tag: byte }),
    })
}

pub fn assignment_operator(byte: u8, at: usize) -> Result<AssignmentOperator, FormatError> {
    Ok(match byte {
        0 => AssignmentOperator::Assign,
        1 => AssignmentOperator::AddAssign,
        2 => AssignmentOperator::SubtractAssign,
        3 => AssignmentOperator::MultiplyAssign,
        4 => AssignmentOperator::DivideAssign,
        5 => AssignmentOperator::RemainderAssign,
        6 => AssignmentOperator::ExponentAssign,
        7 => AssignmentOperator::ShiftLeftAssign,
        8 => AssignmentOperator::ShiftRightAssign,
        9 => AssignmentOperator::UnsignedShiftRightAssign,
        10 => AssignmentOperator::BitwiseAndAssign,
        11 => AssignmentOperator::BitwiseOrAssign,
        12 => AssignmentOperator::BitwiseXorAssign,
        13 => AssignmentOperator::LogicalAndAssign,
        14 => AssignmentOperator::LogicalOrAssign,
        15 => AssignmentOperator::NullishAssign,
        _ => return Err(FormatError::BadTag { at, tag: byte }),
    })
}
