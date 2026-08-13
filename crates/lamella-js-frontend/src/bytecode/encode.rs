//! The encoder: an `ast::Program` in, an artifact out. **Host-side, build-time, never on a device.**

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::ast::*;
use crate::string_value::JsString;
use crate::source::Span;

use super::census::Stats;
use lamella_js_bytecode::format::{
    checksum, uvarint_len, write_uvarint, zigzag, Header, Tag, FLAG_SOURCE, FN_ASYNC, FN_GENERATOR,
    FN_NO_ARGUMENTS, FN_STRICT, FORMAT_VERSION, HEADER_LEN, MIN_READER, NO_STRING,
};

/// String pool kinds. Keeps a UTF-8 identifier and a UTF-16 String value in separate entries even
/// when their bytes would collide, so a lookup can never hand back the wrong text type.
const KIND_UTF8: u8 = 0;
const KIND_UTF16: u8 = 1;

/// What to put in the artifact besides the tree.
///
/// **THE DIAGNOSTIC DECISION LIVES HERE, AND IT WAS MADE BEFORE THE ENCODER EXISTED** -- because
/// retrofitting positions is the misery `source.rs` was written to avoid, and because the
/// neighbouring Python tier ships a format carrying neither line numbers nor a filename, so a
/// program shipped without its source cannot produce a diagnostic that points at anything.
///
/// The floor is not optional: **every node carries its span, always.** On top of that a build
/// chooses what it can afford:
///
/// - `filename` alone: a few bytes, and every diagnostic can at least name the file and an offset
///   a host-side tool resolves against the original.
/// - `source` as well: one flash byte per source byte -- and, being part of the artifact, **zero
///   RAM** -- after which the device can quote the offending line with no host in the loop.
///
/// The REPL, the debugger and the conformance runner all consume positions, so the floor is set at
/// what they need rather than at what is smallest.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options<'a> {
    /// The source text, embedded so diagnostics can quote it. Costs 1 flash byte per source byte.
    pub source: Option<&'a str>,
    /// The name the program was compiled from.
    pub filename: Option<&'a str>,
}

impl<'a> Options<'a> {
    /// Spans only: the smallest artifact that still gives every diagnostic an offset.
    #[must_use]
    pub fn minimal() -> Self {
        Self { source: None, filename: None }
    }

    /// Everything a diagnostic could want, at 1 flash byte per source byte.
    #[must_use]
    pub fn with_source(source: &'a str, filename: &'a str) -> Self {
        Self { source: Some(source), filename: Some(filename) }
    }
}

/// Encodes a program.
#[must_use]
pub fn encode(program: &Program, options: &Options<'_>) -> Vec<u8> {
    encode_with_stats(program, options).0
}

/// Encodes a program and reports where its bytes went. See `census.rs`.
#[must_use]
pub fn encode_with_stats(program: &Program, options: &Options<'_>) -> (Vec<u8>, Stats) {
    let mut encoder = Encoder::default();

    let root_bytes = encoder.program(program);
    let name = options.filename.map(|name| encoder.intern(KIND_UTF8, name.as_bytes()));

    let mut out = Vec::with_capacity(HEADER_LEN + root_bytes.len() + 256);
    out.resize(HEADER_LEN, 0);

    let root = out.len() as u32;
    out.extend_from_slice(&root_bytes);

    let mut blob_offsets = Vec::with_capacity(encoder.blobs.len());
    for blob in &encoder.blobs {
        blob_offsets.push(out.len() as u32);
        write_uvarint(&mut out, blob.len() as u64);
        out.extend_from_slice(blob);
    }
    let str_dir = out.len() as u32;
    for offset in &blob_offsets {
        out.extend_from_slice(&offset.to_le_bytes());
    }

    let (src, src_len, flags) = match options.source {
        Some(text) => {
            let at = out.len() as u32;
            out.extend_from_slice(text.as_bytes());
            (at, text.len() as u32, FLAG_SOURCE)
        }
        None => (0, 0, 0),
    };

    let header = Header {
        version: FORMAT_VERSION,
        min_reader: MIN_READER,
        header_len: HEADER_LEN as u16,
        flags,
        engine: crate::ENGINE_VERSION,
        min_engine: crate::ENGINE_VERSION,
        total_len: out.len() as u32,
        root,
        str_dir,
        str_count: blob_offsets.len() as u32,
        src,
        src_len,
        name: name.unwrap_or(NO_STRING),
        checksum: checksum(&out[HEADER_LEN..]),
    };
    header.write(&mut out[..HEADER_LEN]);
    encoder.stats.arena_bytes = root_bytes.len() as u32;
    (out, encoder.stats)
}

#[derive(Default)]
struct Encoder {
    ids: BTreeMap<(u8, Vec<u8>), u32>,
    blobs: Vec<Vec<u8>>,
    stats: Stats,
    /// Whether the function currently being emitted has named `arguments` yet.
    ///
    /// **THE ENCODER IS THE ONLY EXHAUSTIVE WALK OF THE TREE THERE IS, WHICH IS WHY THE ANSWER
    /// IS COMPUTED HERE.** A separate scan would have to visit every node kind, and one it forgot
    /// would clear [`FN_NO_ARGUMENTS`] for a function that DOES use `arguments` -- a program that
    /// then fails with `arguments is not defined`. Riding on the emission cannot miss a node,
    /// because a node the encoder did not emit is not in the artifact and cannot be evaluated.
    saw_arguments: bool,
}

impl Encoder {
    /// Wraps a payload as a complete node: tag, length, bytes -- and records what that cost.
    ///
    /// The census is taken HERE rather than by a later walk over the artifact, because this is
    /// the only place that knows what each byte is for. See `census.rs`.
    fn node(&mut self, tag: Tag, payload: Vec<u8>) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 5);
        out.push(tag as u8);
        write_uvarint(&mut out, payload.len() as u64);
        let len_field = out.len() - 1;
        out.extend_from_slice(&payload);

        let index = tag as u8 as usize;
        self.stats.counts[index] += 1;
        self.stats.bytes[index] += out.len() as u32;
        self.stats.nodes += 1;
        self.stats.tag_bytes += 1;
        self.stats.len_field_bytes += len_field as u32;
        out
    }

    /// `(start, end - start)`. A node's extent is small where its offset is not, so the second
    /// varint is almost always one byte.
    fn put_span(&mut self, out: &mut Vec<u8>, span: Span) {
        let before = out.len();
        put_u32(out, span.start as u32);
        put_u32(out, span.end.saturating_sub(span.start) as u32);
        self.stats.span_bytes += (out.len() - before) as u32;
    }
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    write_uvarint(out, u64::from(value));
}

fn put_bool(out: &mut Vec<u8>, value: bool) {
    out.push(u8::from(value));
}

/// Whether an `f64` is exactly a small integer, and therefore cheaper as a zigzag varint.
///
/// **THE TEST IS ON BITS, NOT ON VALUE, AND `-0.0` IS WHY.** `-0.0 as i64` is `0`, and `0` back
/// as an `f64` is `+0.0` -- equal by `==`, a different number in this language. `Object.is(-0, 0)`
/// is false and `1 / -0` is `-Infinity`, so a value-equality test here would silently rewrite a
/// program. Comparing bit patterns also disposes of NaN and the infinities for free, since none of
/// them survives the round trip through `i64` either.
fn small_int(value: f64) -> Option<i64> {
    let as_int = value as i64;
    let exact = (as_int as f64).to_bits() == value.to_bits();
    if exact && as_int >= i64::from(i32::MIN) && as_int <= i64::from(i32::MAX) {
        Some(as_int)
    } else {
        None
    }
}

impl Encoder {
    fn intern(&mut self, kind: u8, bytes: &[u8]) -> u32 {
        self.stats.string_bytes_undeduped += bytes.len() as u32;
        let key = (kind, bytes.to_vec());
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        let id = self.blobs.len() as u32;
        self.blobs.push(bytes.to_vec());
        self.ids.insert(key, id);
        self.stats.strings += 1;
        self.stats.string_bytes += (bytes.len() + uvarint_len(bytes.len() as u64)) as u32;
        id
    }

    fn put_str(&mut self, out: &mut Vec<u8>, text: &str) {
        let id = self.intern(KIND_UTF8, text.as_bytes());
        put_u32(out, id);
    }

    fn put_opt_str(&mut self, out: &mut Vec<u8>, text: Option<&alloc::string::String>) {
        match text {
            Some(text) => {
                put_bool(out, true);
                self.put_str(out, text);
            }
            None => put_bool(out, false),
        }
    }

    /// UTF-16 code units, little-endian, lone surrogates included -- the property `JsString`
    /// exists to preserve and the one a UTF-8 pool would quietly destroy.
    fn put_jsstr(&mut self, out: &mut Vec<u8>, text: &JsString) {
        let mut bytes = Vec::with_capacity(text.units().len() * 2);
        for unit in text.units() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let id = self.intern(KIND_UTF16, &bytes);
        put_u32(out, id);
    }


    fn program(&mut self, program: &Program) -> Vec<u8> {
        let mut p = Vec::new();
        self.put_span(&mut p, program.span);
        put_bool(&mut p, program.strict);
        self.put_stmts(&mut p, &program.body);
        self.node(Tag::Program, p)
    }

    fn put_stmts(&mut self, out: &mut Vec<u8>, body: &[Statement]) {
        put_u32(out, body.len() as u32);
        for statement in body {
            let bytes = self.stmt(statement);
            out.extend_from_slice(&bytes);
        }
    }

    fn stmt(&mut self, statement: &Statement) -> Vec<u8> {
        let mut p = Vec::new();
        match statement {
            Statement::Expression { expression, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(expression);
                p.extend_from_slice(&bytes);
                self.node(Tag::StmtExpression, p)
            }
            Statement::Block { body, span } => {
                self.put_span(&mut p, *span);
                self.put_stmts(&mut p, body);
                self.node(Tag::StmtBlock, p)
            }
            Statement::Empty { span } => {
                self.put_span(&mut p, *span);
                self.node(Tag::StmtEmpty, p)
            }
            Statement::Declaration { kind, declarations, span } => {
                self.put_span(&mut p, *span);
                p.push(declaration_kind(*kind));
                self.put_declarators(&mut p, declarations);
                self.node(Tag::StmtDeclaration, p)
            }
            Statement::If { test, consequent, alternate, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(test);
                p.extend_from_slice(&bytes);
                let bytes = self.stmt(consequent);
                p.extend_from_slice(&bytes);
                match alternate {
                    Some(statement) => {
                        put_bool(&mut p, true);
                        let bytes = self.stmt(statement);
                        p.extend_from_slice(&bytes);
                    }
                    None => put_bool(&mut p, false),
                }
                self.node(Tag::StmtIf, p)
            }
            Statement::While { test, body, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(test);
                p.extend_from_slice(&bytes);
                let bytes = self.stmt(body);
                p.extend_from_slice(&bytes);
                self.node(Tag::StmtWhile, p)
            }
            Statement::With { object, body, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(object);
                p.extend_from_slice(&bytes);
                let bytes = self.stmt(body);
                p.extend_from_slice(&bytes);
                self.node(Tag::StmtWith, p)
            }
            Statement::DoWhile { body, test, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.stmt(body);
                p.extend_from_slice(&bytes);
                let bytes = self.expr(test);
                p.extend_from_slice(&bytes);
                self.node(Tag::StmtDoWhile, p)
            }
            Statement::For { init, test, update, body, span } => {
                self.put_span(&mut p, *span);
                match init {
                    Some(init) => {
                        put_bool(&mut p, true);
                        let bytes = self.for_init(init);
                        p.extend_from_slice(&bytes);
                    }
                    None => put_bool(&mut p, false),
                }
                self.put_opt_expr(&mut p, test.as_ref());
                self.put_opt_expr(&mut p, update.as_ref());
                let bytes = self.stmt(body);
                p.extend_from_slice(&bytes);
                self.node(Tag::StmtFor, p)
            }
            Statement::ForIn { left, right, body, span } => {
                self.for_each(&mut p, left, right, body, *span);
                self.node(Tag::StmtForIn, p)
            }
            Statement::ForOf { left, right, body, span } => {
                self.for_each(&mut p, left, right, body, *span);
                self.node(Tag::StmtForOf, p)
            }
            Statement::Return { argument, span } => {
                self.put_span(&mut p, *span);
                self.put_opt_expr(&mut p, argument.as_ref());
                self.node(Tag::StmtReturn, p)
            }
            Statement::Break { label, span } => {
                self.put_span(&mut p, *span);
                self.put_opt_str(&mut p, label.as_ref());
                self.node(Tag::StmtBreak, p)
            }
            Statement::Continue { label, span } => {
                self.put_span(&mut p, *span);
                self.put_opt_str(&mut p, label.as_ref());
                self.node(Tag::StmtContinue, p)
            }
            Statement::Throw { argument, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(argument);
                p.extend_from_slice(&bytes);
                self.node(Tag::StmtThrow, p)
            }
            Statement::Try { block, handler, finalizer, span } => {
                self.put_span(&mut p, *span);
                self.put_stmts(&mut p, block);
                match handler {
                    Some(clause) => {
                        put_bool(&mut p, true);
                        let bytes = self.catch_clause(clause);
                        p.extend_from_slice(&bytes);
                    }
                    None => put_bool(&mut p, false),
                }
                match finalizer {
                    Some(body) => {
                        put_bool(&mut p, true);
                        self.put_stmts(&mut p, body);
                    }
                    None => put_bool(&mut p, false),
                }
                self.node(Tag::StmtTry, p)
            }
            Statement::Labeled { label, body, span } => {
                self.put_span(&mut p, *span);
                self.put_str(&mut p, label);
                let bytes = self.stmt(body);
                p.extend_from_slice(&bytes);
                self.node(Tag::StmtLabeled, p)
            }
            Statement::Switch { discriminant, cases, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(discriminant);
                p.extend_from_slice(&bytes);
                put_u32(&mut p, cases.len() as u32);
                for case in cases {
                    let bytes = self.switch_case(case);
                    p.extend_from_slice(&bytes);
                }
                self.node(Tag::StmtSwitch, p)
            }
            Statement::Function(function) => {
                let bytes = self.function(function);
                p.extend_from_slice(&bytes);
                self.node(Tag::StmtFunction, p)
            }
            Statement::Class(class) => {
                let bytes = self.class(class);
                p.extend_from_slice(&bytes);
                self.node(Tag::StmtClass, p)
            }
            Statement::Debugger { span } => {
                self.put_span(&mut p, *span);
                self.node(Tag::StmtDebugger, p)
            }
        }
    }

    fn for_each(
        &mut self,
        p: &mut Vec<u8>,
        left: &ForInit,
        right: &Expression,
        body: &Statement,
        span: Span,
    ) {
        self.put_span(p, span);
        let bytes = self.for_init(left);
        p.extend_from_slice(&bytes);
        let bytes = self.expr(right);
        p.extend_from_slice(&bytes);
        let bytes = self.stmt(body);
        p.extend_from_slice(&bytes);
    }

    fn put_opt_expr(&mut self, out: &mut Vec<u8>, expression: Option<&Expression>) {
        match expression {
            Some(expression) => {
                put_bool(out, true);
                let bytes = self.expr(expression);
                out.extend_from_slice(&bytes);
            }
            None => put_bool(out, false),
        }
    }

    fn put_declarators(&mut self, out: &mut Vec<u8>, declarations: &[Declarator]) {
        put_u32(out, declarations.len() as u32);
        for declarator in declarations {
            let bytes = self.declarator(declarator);
            out.extend_from_slice(&bytes);
        }
    }

    fn declarator(&mut self, declarator: &Declarator) -> Vec<u8> {
        let mut p = Vec::new();
        self.put_span(&mut p, declarator.span);
        let bytes = self.pattern(&declarator.target);
        p.extend_from_slice(&bytes);
        self.put_opt_expr(&mut p, declarator.init.as_ref());
        self.node(Tag::Declarator, p)
    }

    fn catch_clause(&mut self, clause: &CatchClause) -> Vec<u8> {
        let mut p = Vec::new();
        self.put_span(&mut p, clause.span);
        match &clause.param {
            Some(pattern) => {
                put_bool(&mut p, true);
                let bytes = self.pattern(pattern);
                p.extend_from_slice(&bytes);
            }
            None => put_bool(&mut p, false),
        }
        self.put_stmts(&mut p, &clause.body);
        self.node(Tag::CatchClause, p)
    }

    fn switch_case(&mut self, case: &SwitchCase) -> Vec<u8> {
        let mut p = Vec::new();
        self.put_span(&mut p, case.span);
        self.put_opt_expr(&mut p, case.test.as_ref());
        self.put_stmts(&mut p, &case.body);
        self.node(Tag::SwitchCase, p)
    }

    fn for_init(&mut self, init: &ForInit) -> Vec<u8> {
        let mut p = Vec::new();
        match init {
            ForInit::Declaration { kind, declarations, span } => {
                self.put_span(&mut p, *span);
                p.push(declaration_kind(*kind));
                self.put_declarators(&mut p, declarations);
                self.node(Tag::ForInitDeclaration, p)
            }
            ForInit::Expression(expression) => {
                let bytes = self.expr(expression);
                p.extend_from_slice(&bytes);
                self.node(Tag::ForInitExpression, p)
            }
            ForInit::Pattern(pattern) => {
                let bytes = self.pattern(pattern);
                p.extend_from_slice(&bytes);
                self.node(Tag::ForInitPattern, p)
            }
        }
    }


    fn expr(&mut self, expression: &Expression) -> Vec<u8> {
        let mut p = Vec::new();
        match expression {
            Expression::Yield { span, .. } => {
                panic!(
                    "a `yield` at {}..{} reached the encoder: the generator transform did not \
                     rewrite it, and there is no tag for one",
                    span.start, span.end
                )
            }
            Expression::Identifier { name, span } => {
                if name == "arguments" {
                    self.saw_arguments = true;
                }
                self.put_span(&mut p, *span);
                self.put_str(&mut p, name);
                self.node(Tag::ExprIdentifier, p)
            }
            Expression::Number { value, span } => {
                self.put_span(&mut p, *span);
                match small_int(*value) {
                    Some(int) => {
                        write_uvarint(&mut p, zigzag(int));
                        self.node(Tag::ExprNumberSmall, p)
                    }
                    None => {
                        p.extend_from_slice(&value.to_le_bytes());
                        self.node(Tag::ExprNumber, p)
                    }
                }
            }
            Expression::String { value, span } => {
                self.put_span(&mut p, *span);
                self.put_jsstr(&mut p, value);
                self.node(Tag::ExprString, p)
            }
            Expression::Boolean { value, span } => {
                self.put_span(&mut p, *span);
                put_bool(&mut p, *value);
                self.node(Tag::ExprBoolean, p)
            }
            Expression::Null { span } => {
                self.put_span(&mut p, *span);
                self.node(Tag::ExprNull, p)
            }
            Expression::This { span } => {
                self.put_span(&mut p, *span);
                self.node(Tag::ExprThis, p)
            }
            Expression::Template { quasis, expressions, span } => {
                self.put_span(&mut p, *span);
                put_u32(&mut p, quasis.len() as u32);
                for quasi in quasis {
                    let bytes = self.template_element(quasi);
                    p.extend_from_slice(&bytes);
                }
                put_u32(&mut p, expressions.len() as u32);
                for expression in expressions {
                    let bytes = self.expr(expression);
                    p.extend_from_slice(&bytes);
                }
                self.node(Tag::ExprTemplate, p)
            }
            Expression::Tagged { tag, quasi, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(tag);
                p.extend_from_slice(&bytes);
                let bytes = self.expr(quasi);
                p.extend_from_slice(&bytes);
                self.node(Tag::ExprTagged, p)
            }
            Expression::RegExp { body, flags, span } => {
                self.put_span(&mut p, *span);
                self.put_str(&mut p, body);
                self.put_str(&mut p, flags);
                self.node(Tag::ExprRegExp, p)
            }
            Expression::Array { elements, span } => {
                self.put_span(&mut p, *span);
                put_u32(&mut p, elements.len() as u32);
                for element in elements {
                    let bytes = self.array_element(element);
                    p.extend_from_slice(&bytes);
                }
                self.node(Tag::ExprArray, p)
            }
            Expression::Object { properties, span } => {
                self.put_span(&mut p, *span);
                put_u32(&mut p, properties.len() as u32);
                for property in properties {
                    let bytes = self.object_property(property);
                    p.extend_from_slice(&bytes);
                }
                self.node(Tag::ExprObject, p)
            }
            Expression::Function(function) => {
                let bytes = self.function(function);
                p.extend_from_slice(&bytes);
                self.node(Tag::ExprFunction, p)
            }
            Expression::Arrow(arrow) => {
                let bytes = self.arrow(arrow);
                p.extend_from_slice(&bytes);
                self.node(Tag::ExprArrow, p)
            }
            Expression::Class(class) => {
                let bytes = self.class(class);
                p.extend_from_slice(&bytes);
                self.node(Tag::ExprClass, p)
            }
            Expression::Super { span } => {
                self.put_span(&mut p, *span);
                self.node(Tag::ExprSuper, p)
            }
            Expression::NewTarget { span } => {
                self.put_span(&mut p, *span);
                self.node(Tag::ExprNewTarget, p)
            }
            Expression::Unary { operator, argument, span } => {
                self.put_span(&mut p, *span);
                p.push(unary_operator(*operator));
                let bytes = self.expr(argument);
                p.extend_from_slice(&bytes);
                self.node(Tag::ExprUnary, p)
            }
            Expression::Update { operator, prefix, argument, span } => {
                self.put_span(&mut p, *span);
                p.push(update_operator(*operator));
                put_bool(&mut p, *prefix);
                let bytes = self.expr(argument);
                p.extend_from_slice(&bytes);
                self.node(Tag::ExprUpdate, p)
            }
            Expression::Binary { operator, left, right, span } => {
                self.put_span(&mut p, *span);
                p.push(binary_operator(*operator));
                let bytes = self.expr(left);
                p.extend_from_slice(&bytes);
                let bytes = self.expr(right);
                p.extend_from_slice(&bytes);
                self.node(Tag::ExprBinary, p)
            }
            Expression::Logical { operator, left, right, span } => {
                self.put_span(&mut p, *span);
                p.push(logical_operator(*operator));
                let bytes = self.expr(left);
                p.extend_from_slice(&bytes);
                let bytes = self.expr(right);
                p.extend_from_slice(&bytes);
                self.node(Tag::ExprLogical, p)
            }
            Expression::Assignment { operator, target, value, span } => {
                self.put_span(&mut p, *span);
                p.push(assignment_operator(*operator));
                let bytes = self.assignment_target(target);
                p.extend_from_slice(&bytes);
                let bytes = self.expr(value);
                p.extend_from_slice(&bytes);
                self.node(Tag::ExprAssignment, p)
            }
            Expression::Conditional { test, consequent, alternate, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(test);
                p.extend_from_slice(&bytes);
                let bytes = self.expr(consequent);
                p.extend_from_slice(&bytes);
                let bytes = self.expr(alternate);
                p.extend_from_slice(&bytes);
                self.node(Tag::ExprConditional, p)
            }
            Expression::Call { callee, arguments, optional, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(callee);
                p.extend_from_slice(&bytes);
                put_bool(&mut p, *optional);
                self.put_arguments(&mut p, arguments);
                self.node(Tag::ExprCall, p)
            }
            Expression::New { callee, arguments, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(callee);
                p.extend_from_slice(&bytes);
                self.put_arguments(&mut p, arguments);
                self.node(Tag::ExprNew, p)
            }
            Expression::Member { object, property, optional, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(object);
                p.extend_from_slice(&bytes);
                let bytes = self.member_property(property);
                p.extend_from_slice(&bytes);
                put_bool(&mut p, *optional);
                self.node(Tag::ExprMember, p)
            }
            Expression::Sequence { expressions, span } => {
                self.put_span(&mut p, *span);
                put_u32(&mut p, expressions.len() as u32);
                for expression in expressions {
                    let bytes = self.expr(expression);
                    p.extend_from_slice(&bytes);
                }
                self.node(Tag::ExprSequence, p)
            }
            Expression::Parenthesized { expression, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(expression);
                p.extend_from_slice(&bytes);
                self.node(Tag::ExprParenthesized, p)
            }
        }
    }

    fn put_arguments(&mut self, out: &mut Vec<u8>, arguments: &[Argument]) {
        put_u32(out, arguments.len() as u32);
        for argument in arguments {
            let mut p = Vec::new();
            let bytes = match argument {
                Argument::Expression(expression) => {
                    let bytes = self.expr(expression);
                    p.extend_from_slice(&bytes);
                    self.node(Tag::ArgExpression, p)
                }
                Argument::Spread { argument, span } => {
                    self.put_span(&mut p, *span);
                    let bytes = self.expr(argument);
                    p.extend_from_slice(&bytes);
                    self.node(Tag::ArgSpread, p)
                }
            };
            out.extend_from_slice(&bytes);
        }
    }

    fn array_element(&mut self, element: &ArrayElement) -> Vec<u8> {
        let mut p = Vec::new();
        match element {
            ArrayElement::Hole => self.node(Tag::ElemHole, p),
            ArrayElement::Expression(expression) => {
                let bytes = self.expr(expression);
                p.extend_from_slice(&bytes);
                self.node(Tag::ElemExpression, p)
            }
            ArrayElement::Spread { argument, span, comma_after } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(argument);
                p.extend_from_slice(&bytes);
                put_bool(&mut p, *comma_after);
                self.node(Tag::ElemSpread, p)
            }
        }
    }

    fn object_property(&mut self, property: &ObjectProperty) -> Vec<u8> {
        let mut p = Vec::new();
        match property {
            ObjectProperty::Property { key, value, computed, shorthand, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.property_key(key);
                p.extend_from_slice(&bytes);
                let bytes = self.expr(value);
                p.extend_from_slice(&bytes);
                put_bool(&mut p, *computed);
                put_bool(&mut p, *shorthand);
                self.node(Tag::ObjProperty, p)
            }
            ObjectProperty::CoverInitializedName { name, value, span } => {
                self.put_span(&mut p, *span);
                self.put_str(&mut p, name);
                let bytes = self.expr(value);
                p.extend_from_slice(&bytes);
                self.node(Tag::ObjCoverInitializedName, p)
            }
            ObjectProperty::Method { key, function, kind, computed, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.property_key(key);
                p.extend_from_slice(&bytes);
                let bytes = self.function(function);
                p.extend_from_slice(&bytes);
                p.push(method_kind(*kind));
                put_bool(&mut p, *computed);
                self.node(Tag::ObjMethod, p)
            }
            ObjectProperty::Spread { argument, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(argument);
                p.extend_from_slice(&bytes);
                self.node(Tag::ObjSpread, p)
            }
        }
    }

    fn property_key(&mut self, key: &PropertyKey) -> Vec<u8> {
        let mut p = Vec::new();
        match key {
            PropertyKey::Identifier { name, span } => {
                self.put_span(&mut p, *span);
                self.put_str(&mut p, name);
                self.node(Tag::KeyIdentifier, p)
            }
            PropertyKey::String { value, span } => {
                self.put_span(&mut p, *span);
                self.put_jsstr(&mut p, value);
                self.node(Tag::KeyString, p)
            }
            PropertyKey::Number { value, span } => {
                self.put_span(&mut p, *span);
                p.extend_from_slice(&value.to_le_bytes());
                self.node(Tag::KeyNumber, p)
            }
            PropertyKey::Computed { expression, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(expression);
                p.extend_from_slice(&bytes);
                self.node(Tag::KeyComputed, p)
            }
            PropertyKey::Private { name, span } => {
                self.put_span(&mut p, *span);
                self.put_str(&mut p, name);
                self.node(Tag::KeyPrivate, p)
            }
        }
    }

    fn member_property(&mut self, property: &MemberProperty) -> Vec<u8> {
        let mut p = Vec::new();
        match property {
            MemberProperty::Identifier { name, span } => {
                self.put_span(&mut p, *span);
                self.put_str(&mut p, name);
                self.node(Tag::MemberIdentifier, p)
            }
            MemberProperty::Computed { expression, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(expression);
                p.extend_from_slice(&bytes);
                self.node(Tag::MemberComputed, p)
            }
            MemberProperty::Private { name, span } => {
                self.put_span(&mut p, *span);
                self.put_str(&mut p, name);
                self.node(Tag::MemberPrivate, p)
            }
        }
    }

    fn template_element(&mut self, element: &TemplateElement) -> Vec<u8> {
        let mut p = Vec::new();
        self.put_span(&mut p, element.span);
        match &element.cooked {
            Some(cooked) => {
                put_bool(&mut p, true);
                self.put_jsstr(&mut p, cooked);
            }
            None => put_bool(&mut p, false),
        }
        self.put_str(&mut p, &element.raw);
        self.node(Tag::TemplateElement, p)
    }


    fn pattern(&mut self, pattern: &Pattern) -> Vec<u8> {
        let mut p = Vec::new();
        match pattern {
            Pattern::Identifier { name, span } => {
                self.put_span(&mut p, *span);
                self.put_str(&mut p, name);
                self.node(Tag::PatIdentifier, p)
            }
            Pattern::Member { object, property, optional, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.expr(object);
                p.extend_from_slice(&bytes);
                let bytes = self.member_property(property);
                p.extend_from_slice(&bytes);
                put_bool(&mut p, *optional);
                self.node(Tag::PatMember, p)
            }
            Pattern::Array { elements, rest, span } => {
                self.put_span(&mut p, *span);
                put_u32(&mut p, elements.len() as u32);
                for element in elements {
                    match element {
                        Some(pattern) => {
                            put_bool(&mut p, true);
                            let bytes = self.pattern(pattern);
                            p.extend_from_slice(&bytes);
                        }
                        None => put_bool(&mut p, false),
                    }
                }
                self.put_opt_pattern(&mut p, rest.as_deref());
                self.node(Tag::PatArray, p)
            }
            Pattern::Object { properties, rest, span } => {
                self.put_span(&mut p, *span);
                put_u32(&mut p, properties.len() as u32);
                for property in properties {
                    let bytes = self.object_pattern_property(property);
                    p.extend_from_slice(&bytes);
                }
                self.put_opt_pattern(&mut p, rest.as_deref());
                self.node(Tag::PatObject, p)
            }
            Pattern::Default { target, value, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.pattern(target);
                p.extend_from_slice(&bytes);
                let bytes = self.expr(value);
                p.extend_from_slice(&bytes);
                self.node(Tag::PatDefault, p)
            }
            Pattern::Rest { argument, span } => {
                self.put_span(&mut p, *span);
                let bytes = self.pattern(argument);
                p.extend_from_slice(&bytes);
                self.node(Tag::PatRest, p)
            }
        }
    }

    fn put_opt_pattern(&mut self, out: &mut Vec<u8>, pattern: Option<&Pattern>) {
        match pattern {
            Some(pattern) => {
                put_bool(out, true);
                let bytes = self.pattern(pattern);
                out.extend_from_slice(&bytes);
            }
            None => put_bool(out, false),
        }
    }

    fn object_pattern_property(&mut self, property: &ObjectPatternProperty) -> Vec<u8> {
        let mut p = Vec::new();
        self.put_span(&mut p, property.span);
        let bytes = self.property_key(&property.key);
        p.extend_from_slice(&bytes);
        let bytes = self.pattern(&property.value);
        p.extend_from_slice(&bytes);
        put_bool(&mut p, property.computed);
        put_bool(&mut p, property.shorthand);
        self.node(Tag::ObjectPatternProperty, p)
    }

    fn assignment_target(&mut self, target: &AssignmentTarget) -> Vec<u8> {
        let mut p = Vec::new();
        match target {
            AssignmentTarget::Pattern { pattern, parenthesized } => {
                let bytes = self.pattern(pattern);
                p.extend_from_slice(&bytes);
                put_bool(&mut p, *parenthesized);
                self.node(Tag::TargetPattern, p)
            }
            AssignmentTarget::Invalid(expression) => {
                let bytes = self.expr(expression);
                p.extend_from_slice(&bytes);
                self.node(Tag::TargetInvalid, p)
            }
        }
    }


    fn function(&mut self, function: &Function) -> Vec<u8> {
        let mut p = Vec::new();
        self.put_span(&mut p, function.span);
        self.put_opt_str(&mut p, function.name.as_ref());
        let outer_saw = core::mem::replace(&mut self.saw_arguments, false);
        put_u32(&mut p, function.params.len() as u32);
        for param in &function.params {
            let bytes = self.pattern(param);
            p.extend_from_slice(&bytes);
        }
        self.put_stmts(&mut p, &function.body);
        let uses_arguments = self.saw_arguments;
        self.saw_arguments = outer_saw;
        let mut flags = 0u8;
        if function.is_generator {
            flags |= FN_GENERATOR;
        }
        if function.is_async {
            flags |= FN_ASYNC;
        }
        if function.strict {
            flags |= FN_STRICT;
        }
        if !uses_arguments {
            flags |= FN_NO_ARGUMENTS;
        }
        p.push(flags);
        self.node(Tag::Function, p)
    }

    fn arrow(&mut self, arrow: &Arrow) -> Vec<u8> {
        let mut p = Vec::new();
        self.put_span(&mut p, arrow.span);
        put_u32(&mut p, arrow.params.len() as u32);
        for param in &arrow.params {
            let bytes = self.pattern(param);
            p.extend_from_slice(&bytes);
        }
        let body = match &arrow.body {
            ArrowBody::Expression(expression) => {
                let mut b = Vec::new();
                let bytes = self.expr(expression);
                b.extend_from_slice(&bytes);
                self.node(Tag::ArrowBodyExpression, b)
            }
            ArrowBody::Block(body) => {
                let mut b = Vec::new();
                self.put_stmts(&mut b, body);
                self.node(Tag::ArrowBodyBlock, b)
            }
        };
        p.extend_from_slice(&body);
        let mut flags = 0u8;
        if arrow.is_async {
            flags |= FN_ASYNC;
        }
        if arrow.strict {
            flags |= FN_STRICT;
        }
        p.push(flags);
        self.node(Tag::Arrow, p)
    }

    fn class(&mut self, class: &Class) -> Vec<u8> {
        let mut p = Vec::new();
        self.put_span(&mut p, class.span);
        self.put_opt_str(&mut p, class.name.as_ref());
        match &class.heritage {
            Some(expression) => {
                put_bool(&mut p, true);
                let bytes = self.expr(expression);
                p.extend_from_slice(&bytes);
            }
            None => put_bool(&mut p, false),
        }
        put_u32(&mut p, class.members.len() as u32);
        for member in &class.members {
            let bytes = self.class_member(member);
            p.extend_from_slice(&bytes);
        }
        self.node(Tag::Class, p)
    }

    fn class_member(&mut self, member: &ClassMember) -> Vec<u8> {
        let mut p = Vec::new();
        self.put_span(&mut p, member.span);
        let bytes = self.property_key(&member.key);
        p.extend_from_slice(&bytes);
        p.push(method_kind(member.kind));
        put_bool(&mut p, member.is_static);
        put_bool(&mut p, member.computed);
        let bytes = self.function(&member.function);
        p.extend_from_slice(&bytes);
        self.node(Tag::ClassMember, p)
    }
}


fn declaration_kind(kind: DeclarationKind) -> u8 {
    match kind {
        DeclarationKind::Var => 0,
        DeclarationKind::Let => 1,
        DeclarationKind::Const => 2,
    }
}

fn method_kind(kind: MethodKind) -> u8 {
    match kind {
        MethodKind::Normal => 0,
        MethodKind::Get => 1,
        MethodKind::Set => 2,
    }
}

fn unary_operator(operator: UnaryOperator) -> u8 {
    match operator {
        UnaryOperator::Minus => 0,
        UnaryOperator::Plus => 1,
        UnaryOperator::Not => 2,
        UnaryOperator::BitwiseNot => 3,
        UnaryOperator::TypeOf => 4,
        UnaryOperator::Void => 5,
        UnaryOperator::Delete => 6,
        UnaryOperator::Await => 7,
    }
}

fn update_operator(operator: UpdateOperator) -> u8 {
    match operator {
        UpdateOperator::Increment => 0,
        UpdateOperator::Decrement => 1,
    }
}

fn binary_operator(operator: BinaryOperator) -> u8 {
    match operator {
        BinaryOperator::Add => 0,
        BinaryOperator::Subtract => 1,
        BinaryOperator::Multiply => 2,
        BinaryOperator::Divide => 3,
        BinaryOperator::Remainder => 4,
        BinaryOperator::Exponent => 5,
        BinaryOperator::Equal => 6,
        BinaryOperator::NotEqual => 7,
        BinaryOperator::StrictEqual => 8,
        BinaryOperator::StrictNotEqual => 9,
        BinaryOperator::Less => 10,
        BinaryOperator::LessEqual => 11,
        BinaryOperator::Greater => 12,
        BinaryOperator::GreaterEqual => 13,
        BinaryOperator::ShiftLeft => 14,
        BinaryOperator::ShiftRight => 15,
        BinaryOperator::UnsignedShiftRight => 16,
        BinaryOperator::BitwiseAnd => 17,
        BinaryOperator::BitwiseOr => 18,
        BinaryOperator::BitwiseXor => 19,
        BinaryOperator::In => 20,
        BinaryOperator::InstanceOf => 21,
    }
}

fn logical_operator(operator: LogicalOperator) -> u8 {
    match operator {
        LogicalOperator::And => 0,
        LogicalOperator::Or => 1,
        LogicalOperator::NullishCoalescing => 2,
    }
}

fn assignment_operator(operator: AssignmentOperator) -> u8 {
    match operator {
        AssignmentOperator::Assign => 0,
        AssignmentOperator::AddAssign => 1,
        AssignmentOperator::SubtractAssign => 2,
        AssignmentOperator::MultiplyAssign => 3,
        AssignmentOperator::DivideAssign => 4,
        AssignmentOperator::RemainderAssign => 5,
        AssignmentOperator::ExponentAssign => 6,
        AssignmentOperator::ShiftLeftAssign => 7,
        AssignmentOperator::ShiftRightAssign => 8,
        AssignmentOperator::UnsignedShiftRightAssign => 9,
        AssignmentOperator::BitwiseAndAssign => 10,
        AssignmentOperator::BitwiseOrAssign => 11,
        AssignmentOperator::BitwiseXorAssign => 12,
        AssignmentOperator::LogicalAndAssign => 13,
        AssignmentOperator::LogicalOrAssign => 14,
        AssignmentOperator::NullishAssign => 15,
    }
}
