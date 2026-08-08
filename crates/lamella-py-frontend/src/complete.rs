//! Positional completion (the IntelliSense engine) for Python, clause-agnostic so the in-browser
//! IDE and an editor extension drive the same logic rather than each growing its own.

use crate::ast::{Expr, FuncDef, ModuleAst, ParamDef, Stmt};
use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// What a completion names, so an editor can choose an icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    /// An imported module.
    Module,
    /// A module-level or nested function.
    Function,
    /// A class.
    Class,
    /// A method of a class.
    Method,
    /// An attribute of a class or instance.
    Field,
    /// A local variable, or a module-level binding.
    Local,
    /// A parameter of the enclosing function.
    Parameter,
    /// A language keyword.
    Keyword,
}

impl CompletionKind {
    /// The wire spelling an editor receives, matching the C# engine's item shape.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CompletionKind::Module => "module",
            CompletionKind::Function => "function",
            CompletionKind::Class => "class",
            CompletionKind::Method => "method",
            CompletionKind::Field => "field",
            CompletionKind::Local => "local",
            CompletionKind::Parameter => "parameter",
            CompletionKind::Keyword => "keyword",
        }
    }
}

/// One completion suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// The text shown in the list.
    pub label: String,
    /// What the item names.
    pub kind: CompletionKind,
    /// A short hint shown beside the label; empty when there is nothing useful to say.
    pub detail: String,
    /// The text inserted when the item is chosen. The bare label for most items; a callable
    /// inserts a trailing `(` so the editor can follow with signature help.
    pub insert_text: String,
}

impl CompletionItem {
    fn new(label: &str, kind: CompletionKind, detail: &str) -> CompletionItem {
        let insert_text = match kind {
            CompletionKind::Function | CompletionKind::Method | CompletionKind::Class => {
                format!("{label}(")
            }
            _ => label.to_owned(),
        };
        CompletionItem {
            label: label.to_owned(),
            kind,
            detail: detail.to_owned(),
            insert_text,
        }
    }
}

/// The keywords this front end accepts, offered as completions.
///
/// Deliberately NOT "Python's keywords": it is the set the lexer recognizes, so the editor cannot
/// suggest a construct the compiler would then refuse. `async`/`await` are here because they became
/// real keywords when async landed; the soft keywords (`match`, `case`, `_`) are absent because they
/// are ordinary identifiers outside their own statement and offering them everywhere would be wrong.
const KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield",
];

/// Completions for the caret at byte `offset` in `source`.
///
/// After `self.` inside a method, or `Name.` where `Name` is a class defined in this file, the
/// members of that class. Otherwise the names in lexical scope at the caret -- the enclosing
/// function's parameters and locals, the module's own definitions and imports, and the keywords --
/// filtered by whatever partial identifier the caret sits in.
///
/// `offset` is a BYTE offset into the UTF-8 source. An offset past the end, or inside a multi-byte
/// character, is clamped to a boundary rather than refused.
///
/// Never fails. Unparseable source yields the best list still derivable, and in the worst case an
/// empty one.
#[must_use]
pub fn complete(source: &str, offset: usize) -> Vec<CompletionItem> {
    let caret = Caret::at(source, offset);
    let module = parse_repaired(source, &caret);

    let mut items = match (&caret.receiver, &module) {
        (Some(receiver), Some(module)) => member_completions(module, receiver, caret.offset, source),
        (Some(_), None) => Vec::new(),
        (None, Some(module)) => scope_completions(module, caret.offset, source),
        (None, None) => fallback_completions(source),
    };

    if !caret.prefix.is_empty() {
        items.retain(|item| item.label.starts_with(&caret.prefix));
    }
    items.sort_by(|a, b| a.label.cmp(&b.label).then(a.kind.as_str().cmp(b.kind.as_str())));
    items.dedup_by(|a, b| a.label == b.label && a.kind == b.kind);
    items
}

/// What the caret sits in, read from the source bytes so it survives source that will not parse.
struct Caret {
    /// The byte offset, clamped into the source and onto a character boundary.
    offset: usize,
    /// The partial identifier the caret is inside (`fo|` -> `fo`), else empty.
    prefix: String,
    /// The receiver text before a trailing `.` (`self.fo|` -> `self`), else `None`.
    receiver: Option<String>,
}

impl Caret {
    fn at(source: &str, offset: usize) -> Caret {
        let mut offset = offset.min(source.len());
        while offset > 0 && !source.is_char_boundary(offset) {
            offset -= 1;
        }
        let bytes = source.as_bytes();
        let mut start = offset;
        while start > 0 && is_ident_byte(bytes[start - 1]) {
            start -= 1;
        }
        let prefix = source[start..offset].to_string();
        let receiver = if start > 0 && bytes[start - 1] == b'.' {
            let end = start - 1;
            let mut begin = end;
            while begin > 0 && (is_ident_byte(bytes[begin - 1]) || bytes[begin - 1] == b'.') {
                begin -= 1;
            }
            let text = source[begin..end].trim();
            (!text.is_empty()).then(|| text.to_string())
        } else {
            None
        };
        Caret {
            offset,
            prefix,
            receiver,
        }
    }
}

fn is_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric() || byte >= 0x80
}

/// Parse `source`, first as it stands and then with the caret's partial member repaired.
///
/// `x = math.` is not a statement, so a file being typed into commonly does not parse at the caret
/// while parsing perfectly everywhere else -- and everywhere else is where the declarations are.
/// Substituting a placeholder for the partial member recovers the whole tree. The placeholder is a
/// name no source can contain, so it can never itself be offered.
fn parse_repaired(source: &str, caret: &Caret) -> Option<ModuleAst> {
    let parse = |text: &str| crate::parser::parse(crate::lexer::tokenize(text).ok()?).ok();
    if let Some(module) = parse(source) {
        return Some(module);
    }
    if caret.receiver.is_some() || !caret.prefix.is_empty() {
        let mut repaired = String::with_capacity(source.len() + 8);
        let start = caret.offset - caret.prefix.len();
        repaired.push_str(&source[..start]);
        repaired.push_str("__lamella_caret");
        repaired.push_str(&source[caret.offset..]);
        if let Some(module) = parse(&repaired) {
            return Some(module);
        }
    }
    None
}

/// The members of `receiver` when it is a class defined in this file, or `self` inside one of its
/// methods. Any other receiver is dynamically typed and has no answer without inference.
fn member_completions(
    module: &ModuleAst,
    receiver: &str,
    offset: usize,
    source: &str,
) -> Vec<CompletionItem> {
    let class_name = if receiver == "self" {
        match enclosing_class(offset, source) {
            Some(name) => name,
            None => return Vec::new(),
        }
    } else {
        receiver.to_string()
    };
    let Some(body) = class_body(module, &class_name) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    collect_class_members(body, &mut items);
    items
}

/// A class body's methods, class attributes, and the instance attributes its methods assign to
/// `self` -- which is where most of a Python class's real surface is declared.
fn collect_class_members(body: &[Stmt], items: &mut Vec<CompletionItem>) {
    for stmt in body {
        match undecorated(stmt) {
            Stmt::FuncDef(func) => {
                items.push(CompletionItem::new(
                    &func.name,
                    CompletionKind::Method,
                    &signature(func),
                ));
                collect_self_assignments(&func.body, items);
            }
            Stmt::Assign(assign) => {
                items.push(CompletionItem::new(
                    &assign.target,
                    CompletionKind::Field,
                    "",
                ));
            }
            Stmt::ClassDef { name, .. } => {
                items.push(CompletionItem::new(name, CompletionKind::Class, ""));
            }
            _ => {}
        }
    }
}

/// Attributes assigned through `self` anywhere in a statement list, descending into nested blocks
/// (an attribute set inside an `if` is no less an attribute).
fn collect_self_assignments(body: &[Stmt], items: &mut Vec<CompletionItem>) {
    for stmt in body {
        if let Stmt::SetAttr { obj, attr, .. } = stmt {
            if matches!(obj, Expr::Name(n) if n == "self") {
                items.push(CompletionItem::new(attr, CompletionKind::Field, ""));
            }
        }
        for block in child_blocks(stmt) {
            collect_self_assignments(block, items);
        }
    }
}

/// The names visible at the caret: the enclosing function's parameters and locals, then the
/// module's own top-level bindings, then the keywords.
fn scope_completions(module: &ModuleAst, offset: usize, source: &str) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    if let Some(func) = enclosing_function(&module.body, offset, source) {
        for param in &func.params {
            items.push(CompletionItem::new(
                &param.name,
                CompletionKind::Parameter,
                annotation_text(param),
            ));
        }
        collect_bindings(&func.body, &mut items);
    }
    collect_bindings(&module.body, &mut items);
    for keyword in KEYWORDS {
        items.push(CompletionItem::new(keyword, CompletionKind::Keyword, ""));
    }
    items
}

/// The names a statement list binds, descending into nested blocks but NOT into nested function or
/// class bodies (whose bindings are their own scope, not this one).
fn collect_bindings(body: &[Stmt], items: &mut Vec<CompletionItem>) {
    for stmt in body {
        match undecorated(stmt) {
            Stmt::FuncDef(func) => items.push(CompletionItem::new(
                &func.name,
                CompletionKind::Function,
                &signature(func),
            )),
            Stmt::ClassDef { name, .. } => {
                items.push(CompletionItem::new(name, CompletionKind::Class, ""));
            }
            Stmt::Assign(assign) => {
                items.push(CompletionItem::new(&assign.target, CompletionKind::Local, ""));
            }
            Stmt::Import { modules } => {
                for (module, alias) in modules {
                    let bound = crate::ast::import_bound_name(module, alias);
                    items.push(CompletionItem::new(bound, CompletionKind::Module, module));
                }
            }
            Stmt::ImportFrom { module, names } => {
                for (name, bound) in names {
                    let detail = if name == bound {
                        format!("from {module}")
                    } else {
                        format!("from {module} import {name}")
                    };
                    items.push(CompletionItem::new(bound, CompletionKind::Local, &detail));
                }
            }
            Stmt::For { target, .. } | Stmt::ForIter { target, .. } | Stmt::AsyncFor { target, .. } => {
                items.push(CompletionItem::new(target, CompletionKind::Local, ""));
            }
            Stmt::With { optional_name, .. } | Stmt::AsyncWith { optional_name, .. } => {
                if let Some(name) = optional_name {
                    items.push(CompletionItem::new(name, CompletionKind::Local, ""));
                }
            }
            _ => {}
        }
        if !matches!(undecorated(stmt), Stmt::FuncDef(_) | Stmt::ClassDef { .. }) {
            for block in child_blocks(stmt) {
                collect_bindings(block, items);
            }
        }
    }
}

/// The statement lists nested directly inside `stmt`.
fn child_blocks(stmt: &Stmt) -> Vec<&[Stmt]> {
    match stmt {
        Stmt::If { body, orelse, .. } | Stmt::While { body, orelse, .. } => {
            alloc::vec![body.as_slice(), orelse.as_slice()]
        }
        Stmt::For { body, orelse, .. }
        | Stmt::ForIter { body, orelse, .. }
        | Stmt::AsyncFor { body, orelse, .. } => alloc::vec![body.as_slice(), orelse.as_slice()],
        Stmt::With { body, .. } | Stmt::AsyncWith { body, .. } => alloc::vec![body.as_slice()],
        Stmt::Try {
            body,
            handlers,
            orelse,
            finalbody,
        } => {
            let mut blocks = alloc::vec![body.as_slice(), orelse.as_slice(), finalbody.as_slice()];
            blocks.extend(handlers.iter().map(|h| h.body.as_slice()));
            blocks
        }
        _ => Vec::new(),
    }
}

/// A decorated definition's inner statement, else the statement itself.
fn undecorated(stmt: &Stmt) -> &Stmt {
    match stmt {
        Stmt::Decorated { inner, .. } => inner,
        other => other,
    }
}

/// A `def`'s parameter list rendered for the detail column.
fn signature(func: &FuncDef) -> String {
    let names: Vec<&str> = func.params.iter().map(|p| p.name.as_str()).collect();
    let prefix = if func.is_async { "async def " } else { "def " };
    format!("{prefix}{}({})", func.name, names.join(", "))
}

/// A parameter's annotation rendered for the detail column, when it is a simple name (`int`).
fn annotation_text(param: &ParamDef) -> &str {
    match &param.annotation {
        Some(Expr::Name(name)) => name,
        _ => "",
    }
}

/// The body of the class named `name`, searched at module level and inside functions.
fn class_body<'a>(module: &'a ModuleAst, name: &str) -> Option<&'a [Stmt]> {
    fn search<'a>(body: &'a [Stmt], name: &str) -> Option<&'a [Stmt]> {
        for stmt in body {
            match undecorated(stmt) {
                Stmt::ClassDef { name: found, body, .. } if found == name => {
                    return Some(body.as_slice())
                }
                Stmt::FuncDef(func) => {
                    if let Some(found) = search(&func.body, name) {
                        return Some(found);
                    }
                }
                other => {
                    for block in child_blocks(other) {
                        if let Some(found) = search(block, name) {
                            return Some(found);
                        }
                    }
                }
            }
        }
        None
    }
    search(&module.body, name)
}

/// The name of the class whose body contains the caret, by INDENTATION rather than by span: the AST
/// carries no byte ranges, so the class a `self` belongs to is found the way a reader finds it --
/// the nearest `class` header at a lower indent, above the caret.
fn enclosing_class(offset: usize, source: &str) -> Option<String> {
    header_above(source, offset, "class")
}

/// The function whose body contains the caret, matched by name from the same indentation scan.
fn enclosing_function<'a>(body: &'a [Stmt], offset: usize, source: &str) -> Option<&'a FuncDef> {
    let name = header_above(source, offset, "def")?;
    fn find<'a>(body: &'a [Stmt], name: &str) -> Option<&'a FuncDef> {
        for stmt in body {
            match undecorated(stmt) {
                Stmt::FuncDef(func) => {
                    if func.name == name {
                        return Some(func);
                    }
                    if let Some(found) = find(&func.body, name) {
                        return Some(found);
                    }
                }
                Stmt::ClassDef { body, .. } => {
                    if let Some(found) = find(body, name) {
                        return Some(found);
                    }
                }
                other => {
                    for block in child_blocks(other) {
                        if let Some(found) = find(block, name) {
                            return Some(found);
                        }
                    }
                }
            }
        }
        None
    }
    find(body, &name)
}

/// The name introduced by the nearest `def`/`class` header above `offset` that the caret's line is
/// indented under. Byte-level, so it works on source no parser accepted.
fn header_above(source: &str, offset: usize, keyword: &str) -> Option<String> {
    let head = &source[..offset];
    let caret_indent = indent_of(head.lines().next_back().unwrap_or(""));
    let mut best: Option<(usize, String)> = None;
    for line in head.lines() {
        let indent = indent_of(line);
        let trimmed = line.trim_start();
        let Some(rest) = trimmed
            .strip_prefix("async ")
            .unwrap_or(trimmed)
            .strip_prefix(keyword)
        else {
            continue;
        };
        let Some(name) = rest.strip_prefix(' ') else {
            continue;
        };
        let name: String = name
            .trim_start()
            .chars()
            .take_while(|c| *c == '_' || c.is_alphanumeric())
            .collect();
        if name.is_empty() || indent >= caret_indent {
            continue;
        }
        if best.as_ref().is_none_or(|(depth, _)| indent >= *depth) {
            best = Some((indent, name));
        }
    }
    best.map(|(_, name)| name)
}

/// A line's leading-whitespace width, tabs counted as one column (only the ordering matters here).
fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The names a file defines, scanned from the token stream when the source will not parse at all.
///
/// Deliberately shallow: it offers what the file declares with no scope analysis, because at this
/// point the alternative is an empty list. The keywords are always available, since they do not
/// depend on the file being well formed.
fn fallback_completions(source: &str) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = KEYWORDS
        .iter()
        .map(|k| CompletionItem::new(k, CompletionKind::Keyword, ""))
        .collect();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let rest = trimmed.strip_prefix("async ").unwrap_or(trimmed);
        let (keyword, kind) = if let Some(rest) = rest.strip_prefix("def ") {
            (rest, CompletionKind::Function)
        } else if let Some(rest) = rest.strip_prefix("class ") {
            (rest, CompletionKind::Class)
        } else {
            continue;
        };
        let name: String = keyword
            .trim_start()
            .chars()
            .take_while(|c| *c == '_' || c.is_alphanumeric())
            .collect();
        if !name.is_empty() {
            items.push(CompletionItem::new(&name, kind, ""));
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Complete at the caret marked `|` in `marked`.
    fn at(marked: &str) -> Vec<CompletionItem> {
        let offset = marked.find('|').expect("the test source marks a caret with |");
        let source = marked.replacen('|', "", 1);
        complete(&source, offset)
    }

    fn labels(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    fn kind_of(items: &[CompletionItem], label: &str) -> Option<CompletionKind> {
        items.iter().find(|i| i.label == label).map(|i| i.kind)
    }

    #[test]
    fn a_partial_name_offers_the_definitions_of_the_file() {
        let items = at("def alpha():\n    pass\n\ndef beta():\n    pass\n\nal|\n");
        assert_eq!(labels(&items), vec!["alpha"], "filtered by the typed prefix");
        assert_eq!(kind_of(&items, "alpha"), Some(CompletionKind::Function));
        assert_eq!(items[0].insert_text, "alpha(");
        assert_eq!(items[0].detail, "def alpha()");
    }

    #[test]
    fn parameters_and_locals_of_the_enclosing_function_are_in_scope() {
        let items = at("def f(count, name):\n    total = 0\n    t|\n");
        let found = labels(&items);
        assert!(found.contains(&"total"), "a local: {found:?}");
        assert!(kind_of(&items, "total") == Some(CompletionKind::Local));
        let params = at("def f(count, name):\n    total = 0\n    c|\n");
        assert_eq!(kind_of(&params, "count"), Some(CompletionKind::Parameter));
        let annotated = at("def f(n: int):\n    n|\n");
        assert_eq!(
            annotated.iter().find(|i| i.label == "n").map(|i| i.detail.as_str()),
            Some("int")
        );
    }

    #[test]
    fn self_dot_inside_a_method_offers_the_class_members() {
        let source = "class Robot:\n    def __init__(self):\n        self.speed = 0\n\
                      \n    def drive(self, d):\n        self.|\n";
        let items = at(source);
        let found = labels(&items);
        assert!(found.contains(&"speed"), "an attribute assigned through self: {found:?}");
        assert!(found.contains(&"drive"), "a sibling method: {found:?}");
        assert!(found.contains(&"__init__"), "{found:?}");
        assert_eq!(kind_of(&items, "speed"), Some(CompletionKind::Field));
        assert_eq!(kind_of(&items, "drive"), Some(CompletionKind::Method));
        assert!(!found.contains(&"class"), "no keywords after a receiver: {found:?}");
        assert!(!found.contains(&"Robot"), "{found:?}");
    }

    #[test]
    fn a_class_defined_in_the_file_answers_for_its_own_name() {
        let items = at("class Point:\n    x = 0\n    def norm(self):\n        return 0\n\nPoint.|\n");
        let found = labels(&items);
        assert!(found.contains(&"x") && found.contains(&"norm"), "{found:?}");
        assert!(at("import math\nmath.|\n").is_empty(), "no inference, no invention");
        assert!(at("x = whatever()\nx.|\n").is_empty());
    }

    #[test]
    fn keywords_offered_are_the_ones_this_front_end_accepts() {
        let items = at("as|\n");
        let found = labels(&items);
        assert!(found.contains(&"assert") && found.contains(&"async"));
        assert_eq!(kind_of(&items, "async"), Some(CompletionKind::Keyword));
        assert!(!labels(&at("mat|\n")).contains(&"match"));
    }

    #[test]
    fn broken_source_answers_a_list_rather_than_failing() {
        for marked in [
            "def f(:\n    x|\n",
            "class C\n    def m(self):\n        self.|\n",
            "x = (1 +\ny|\n",
            "if True\n    al|\n",
            "def f():\n    return |\n",
            "|\n",
            "",
            "\u{1f600}|\n",
        ] {
            let offset = marked.find('|').unwrap_or(marked.len());
            let source = marked.replacen('|', "", 1);
            let _ = complete(&source, offset);
        }
        let items = at("def f(:\n    pass\n\ndef alpha():\n    pass\n\nal|\n");
        assert!(labels(&items).contains(&"alpha"), "the fallback still finds a def");
    }

    #[test]
    fn an_out_of_range_or_mid_character_offset_is_clamped_not_refused() {
        let source = "x = 1\n";
        assert!(!complete(source, 9_999).is_empty(), "past the end clamps to the end");
        let emoji = "s = '\u{1f600}'\nx|";
        let _ = complete(emoji, 7);
        assert!(!complete("", 0).is_empty(), "an empty file still offers keywords");
    }

    #[test]
    fn a_nested_function_does_not_leak_its_locals_to_the_module_scope() {
        let items = at("def outer():\n    inner_local = 1\n\ntop = 2\nt|\n");
        let found = labels(&items);
        assert!(found.contains(&"top"), "{found:?}");
        assert!(
            !found.contains(&"inner_local"),
            "a function's locals are its own scope: {found:?}"
        );
    }

    #[test]
    fn imports_and_loop_and_with_targets_are_offered() {
        let items = at("import math\nfrom sys import argv as args\nfor row in rows:\n    r|\n");
        let found = labels(&items);
        assert!(found.contains(&"row"), "a loop target: {found:?}");
        assert_eq!(kind_of(&at("import math\nma|\n"), "math"), Some(CompletionKind::Module));
        let renamed = at("from sys import argv as args\narg|\n");
        assert_eq!(
            renamed.iter().find(|i| i.label == "args").map(|i| i.detail.as_str()),
            Some("from sys import argv"),
            "an `as` rename names the original member"
        );
    }

    #[test]
    fn results_are_sorted_and_carry_no_duplicates() {
        let items = at("x = 1\nx = 2\ndef y():\n    pass\n\n|\n");
        let found = labels(&items);
        let mut sorted = found.clone();
        sorted.sort_unstable();
        assert_eq!(found, sorted, "sorted for a stable list");
        assert_eq!(
            found.iter().filter(|l| **l == "x").count(),
            1,
            "a name bound twice appears once"
        );
    }
}
