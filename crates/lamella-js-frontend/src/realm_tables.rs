//! A total, canonical rendering of the installed realm, and the gate that compares it against a
//! committed copy.

use crate::interpreter::Interpreter;
use crate::object::{Callable, Object, Property, PropertyKey, PropertyKind};
use crate::value::{JsValue, ObjectId};
use crate::{format, String, Vec};

/// Where the committed rendering lives, relative to the package root.
///
/// A path relative to the working directory rather than one built from `CARGO_MANIFEST_DIR`:
/// cargo runs a test binary with its package root as the working directory, and the macro form
/// bakes an ABSOLUTE path into the compiled binary, which stops resolving the moment the worktree
/// is moved. A relative path cannot go stale that way.
const SNAPSHOT_PATH: &str = "realm/installed-realm.txt";

/// Setting this to anything rewrites [`SNAPSHOT_PATH`] instead of comparing against it.
pub(crate) const REWRITE_VARIABLE: &str = "LAMELLA_JS_REALM_REWRITE";

/// The rendering's own version, so a format change is a deliberate, visible act.
const FORMAT: u32 = 1;

/// The whole installed realm, rendered.
///
/// Deterministic by construction: everything it reads is a table walked in index order, and
/// nothing about an address, a clock or a hash reaches the output.
fn snapshot(interpreter: &Interpreter) -> String {
    let census = interpreter.realm_census();
    let objects = census.objects;

    let mut named_total = 0usize;
    let mut element_total = 0usize;
    for id in 0..objects {
        let object = interpreter.object(ObjectId(id as u32));
        named_total += object.named_entries().len();
        element_total += object.element_slots().len();
    }

    let mut out = String::new();
    out.push_str(&format!("lamella-js realm snapshot, format {FORMAT}\n"));
    out.push_str(&format!("objects {objects}\n"));
    out.push_str(&format!("named-properties {named_total}\n"));
    out.push_str(&format!("dense-elements {element_total}\n"));
    out.push_str(&format!("natives {}\n", interpreter.native_registry().len()));
    out.push_str(&format!("symbols {}\n", interpreter.symbol_table().len()));

    out.push_str("\n[symbols]\n");
    for (index, description) in interpreter.symbol_table().iter().enumerate() {
        match description {
            Some(text) => out.push_str(&format!("sym {index} desc {}\n", quoted(text.units()))),
            None => out.push_str(&format!("sym {index} desc none\n")),
        }
    }

    out.push_str("\n[natives]\n");
    for (index, native) in interpreter.native_registry().iter().enumerate() {
        let constructs = if native.construct.is_some() { " construct" } else { "" };
        out.push_str(&format!(
            "nat {index} name {} length {}{constructs}\n",
            quoted_str(&native.name),
            native.length
        ));
    }

    out.push_str("\n[intrinsics]\n");
    out.push_str(&format!("{:#?}\n", interpreter.intrinsics));

    out.push_str("\n[objects]\n");
    for id in 0..objects {
        out.push_str(&object_text(id, interpreter.object(ObjectId(id as u32))));
    }
    out
}

/// One object's every field, its properties and its elements.
///
/// **SEPARATE FROM [`snapshot`] BECAUSE IT IS ALSO THE EMITTER'S ORACLE.** The tables are checked
/// by building an object out of each descriptor and comparing this rendering against the installed
/// object's, which makes the comparison total over the same fields the snapshot covers rather than
/// over whichever ones a second, hand-written comparison remembered.
fn object_text(id: usize, object: &Object) -> String {
    refuse_unexpressable(id, object);

    let mut out = format!(
        "obj {id} proto {} callable {} array {} ext {} primitive {} date {} error {} \
         named {} elements {}\n",
        optional_id(object.prototype),
        callable(object.callable),
        flag(object.is_array),
        flag(object.extensible),
        match &object.primitive {
            Some(value) => value_text(value),
            None => "none".into(),
        },
        match object.date {
            Some(millis) => number(millis),
            None => "none".into(),
        },
        flag(object.error),
        object.named_entries().len(),
        object.element_slots().len(),
    );

    for (index, (key, property)) in object.named_entries().iter().enumerate() {
        out.push_str(&format!(
            "obj {id} named {index} key {} {}\n",
            key_text(key),
            property_text(property)
        ));
    }
    for (index, slot) in object.element_slots().iter().enumerate() {
        match slot {
            None => out.push_str(&format!("obj {id} elem {index} hole\n")),
            Some(property) => {
                out.push_str(&format!("obj {id} elem {index} {}\n", property_text(property)));
            }
        }
    }
    out
}

/// Stops the rendering at an object holding state a flash descriptor cannot carry.
///
/// A `ResidentObject` names nine fields and deliberately cannot express an iterator's state, a
/// collection, a promise, a proxy pair, a compiled pattern or an `arguments` parameter map. Every
/// one is absent from a freshly built realm. **A generator meeting one would emit a descriptor
/// without it and produce a built-in that has lost its brand check** -- so this refuses instead,
/// naming the object and the slot.
fn refuse_unexpressable(id: usize, object: &Object) {
    let slots = [
        ("iterator_state", object.iterator_state.is_some()),
        ("collection", object.collection.is_some()),
        ("binary", object.binary.is_some()),
        ("promise", object.promise.is_some()),
        ("resolver", object.resolver.is_some()),
        ("combinator", object.combinator.is_some()),
        ("capability_parts", object.capability_parts.is_some()),
        ("arguments", object.arguments.is_some()),
        ("proxy", object.proxy.is_some()),
        ("generator", object.generator.is_some()),
        ("regexp", object.regexp.is_some()),
    ];
    for (slot, present) in slots {
        assert!(
            !present,
            "realm object {id} holds `{slot}`, which a flash descriptor cannot carry -- widen \
             `ResidentObject` before generating tables, or the slot is dropped in silence"
        );
    }
}

fn flag(value: bool) -> &'static str {
    if value { "1" } else { "0" }
}

fn optional_id(id: Option<ObjectId>) -> String {
    match id {
        Some(ObjectId(value)) => format!("{value}"),
        None => "none".into(),
    }
}

fn callable(callable: Option<Callable>) -> String {
    match callable {
        None => "none".into(),
        Some(Callable::Native(index)) => format!("native {index}"),
        Some(Callable::Closure(index)) => format!("closure {index}"),
        Some(Callable::Bound(index)) => format!("bound {index}"),
        Some(Callable::Resolver { promise, rejects }) => {
            format!("resolver {} {}", promise.0, flag(rejects))
        }
        Some(Callable::Combinator { state, index, rejects }) => {
            format!("combinator {} {index} {}", state.0, flag(rejects))
        }
        Some(Callable::CapabilityExecutor { state }) => format!("capability-executor {}", state.0),
        Some(Callable::Proxy) => "proxy".into(),
        Some(Callable::ProxyRevoker { proxy }) => format!("proxy-revoker {}", proxy.0),
    }
}

fn key_text(key: &PropertyKey) -> String {
    match key {
        PropertyKey::String(text) => format!("str {}", quoted(text.units())),
        PropertyKey::Symbol(id) => format!("sym {}", id.0),
    }
}

fn value_text(value: &JsValue) -> String {
    match value {
        JsValue::Undefined => "undefined".into(),
        JsValue::Null => "null".into(),
        JsValue::Boolean(value) => format!("bool {}", flag(*value)),
        JsValue::Number(value) => format!("num {}", number(*value)),
        JsValue::String(text) => format!("str {}", quoted(text.units())),
        JsValue::Symbol(id) => format!("sym {}", id.0),
        JsValue::Object(id) => format!("obj {}", id.0),
    }
}

fn property_text(property: &Property) -> String {
    let tail = format!("e {} c {}", flag(property.enumerable), flag(property.configurable));
    match &property.kind {
        PropertyKind::Data { value, writable } => {
            format!("data {} w {} {tail}", value_text(value), flag(*writable))
        }
        PropertyKind::Accessor { get, set } => {
            format!("accessor get {} set {} {tail}", optional_id(*get), optional_id(*set))
        }
    }
}


/// Where the generated tables live, relative to the package root. See [`SNAPSHOT_PATH`].
const TABLES_PATH: &str = "src/realm.rs";

/// One entry of the interned text pool: the code units, and the `static` they were given.
struct Text {
    units: Vec<u16>,
    name: String,
}

/// Every distinct piece of string text the realm needs, in first-encounter order.
///
/// # INTERNING IS NOT A TIDINESS MEASURE, IT IS MOST OF THE SAVING
///
/// Measured over the installed realm: **1,389 string keys reduce to 327 distinct, and 452 string
/// values to 316** -- `name` appears 424 times and `length` 417, because every one of the 413
/// built-in functions carries both. Keys and values overlap heavily as well, since a function's
/// `name` value is usually some other object's key, so the union is **366 pieces of text** standing
/// in for 1,841 references.
///
/// They are `static` arrays rather than `const` slices ON PURPOSE. A `const` is substituted at each
/// use site, and whether the promoted allocations behind 424 substitutions are merged is a decision
/// left to the optimizer; a `static` has exactly one address by definition. When the whole point of
/// the exercise is bytes, the guarantee is worth more than the brevity.
fn intern(interpreter: &Interpreter) -> Vec<Text> {
    let mut pool: Vec<Text> = Vec::new();
    let add = |pool: &mut Vec<Text>, units: &[u16]| {
        if pool.iter().any(|entry| entry.units == units) {
            return;
        }
        let name = text_name(pool.len(), units);
        assert!(
            !pool.iter().any(|entry| entry.name == name),
            "two pieces of realm text were given the same static name `{name}`"
        );
        pool.push(Text { units: units.to_vec(), name });
    };

    for id in 0..interpreter.realm_census().objects {
        let object = interpreter.object(ObjectId(id as u32));
        for (key, property) in object.named_entries() {
            if let PropertyKey::String(text) = key {
                add(&mut pool, text.units());
            }
            if let Some(JsValue::String(text)) = property.data_value() {
                add(&mut pool, text.units());
            }
        }
        for property in object.element_slots().iter().flatten() {
            if let Some(JsValue::String(text)) = property.data_value() {
                add(&mut pool, text.units());
            }
        }
        if let Some(JsValue::String(text)) = &object.primitive {
            add(&mut pool, text.units());
        }
    }
    pool
}

/// A unique, readable name for one interned string.
///
/// The index leads so uniqueness is by construction rather than by a collision check that has to be
/// right; the sanitized text follows so a reader of the generated tables can see what a row is
/// about without chasing the definition.
fn text_name(index: usize, units: &[u16]) -> String {
    let mut tail = String::new();
    for unit in units.iter().take(24) {
        let ch = char::from_u32(u32::from(*unit)).unwrap_or('_');
        tail.push(if ch.is_ascii_alphanumeric() { ch.to_ascii_uppercase() } else { '_' });
    }
    if tail.is_empty() {
        return format!("T{index}_EMPTY");
    }
    format!("T{index}_{tail}")
}

fn interned<'a>(pool: &'a [Text], units: &[u16]) -> &'a str {
    &pool
        .iter()
        .find(|entry| entry.units == units)
        .unwrap_or_else(|| panic!("a piece of realm text was never interned"))
        .name
}

/// The realm as Rust source: the text pool, one named table per object, and the descriptors.
fn generate(interpreter: &Interpreter) -> String {
    let pool = intern(interpreter);
    let objects = interpreter.realm_census().objects;

    let mut out = String::new();
    out.push_str(&generated_header(objects, pool.len()));

    out.push_str("\n// -- The interned text: every string key and string value in the realm.\n\n");
    for entry in &pool {
        let units: Vec<String> = entry.units.iter().map(|unit| format!("0x{unit:04X}")).collect();
        out.push_str(&format!(
            "static {}: [u16; {}] = [{}];\n",
            entry.name,
            entry.units.len(),
            units.join(", ")
        ));
    }

    out.push_str("\n// -- One property table per object that has properties.\n\n");
    for id in 0..objects {
        let object = interpreter.object(ObjectId(id as u32));
        if !object.named_entries().is_empty() {
            out.push_str(&format!(
                "static NAMED_{id}: [(PropertyKey, Property); {}] = [\n",
                object.named_entries().len()
            ));
            for (key, property) in object.named_entries() {
                out.push_str(&format!(
                    "    ({}, {}),\n",
                    key_source(&pool, key),
                    property_source(&pool, property)
                ));
            }
            out.push_str("];\n");
        }
        if !object.element_slots().is_empty() {
            out.push_str(&format!(
                "static ELEMENTS_{id}: [Option<Property>; {}] = [\n",
                object.element_slots().len()
            ));
            for slot in object.element_slots() {
                match slot {
                    None => out.push_str("    None,\n"),
                    Some(property) => {
                        out.push_str(&format!("    Some({}),\n", property_source(&pool, property)));
                    }
                }
            }
            out.push_str("];\n");
        }
    }

    out.push_str(&format!(
        "\n// -- The descriptors, in `ObjectId` order.\n\n\
         #[cfg_attr(not(test), allow(dead_code))]\n\
         pub(crate) static REALM: [ResidentObject; {objects}] = [\n"
    ));
    for id in 0..objects {
        let object = interpreter.object(ObjectId(id as u32));
        refuse_unexpressable(id, object);
        let named = if object.named_entries().is_empty() {
            String::from("&[]")
        } else {
            format!("&NAMED_{id}")
        };
        let elements = if object.element_slots().is_empty() {
            String::from("&[]")
        } else {
            format!("&ELEMENTS_{id}")
        };
        out.push_str(&format!(
            "    ResidentObject {{ prototype: {}, named: {named}, elements: {elements}, \
             callable: {}, is_array: {}, extensible: {}, primitive: {}, date: {}, error: {} }},\n",
            optional_id_source(object.prototype),
            callable_source(object.callable),
            object.is_array,
            object.extensible,
            match &object.primitive {
                Some(value) => format!("Some({})", value_source(&pool, value)),
                None => String::from("None"),
            },
            match object.date {
                Some(millis) => format!("Some({})", number_source(millis)),
                None => String::from("None"),
            },
            object.error,
        ));
    }
    out.push_str("];\n");
    out
}

/// The generated file's own header. It has to say what regenerates it and what checks it.
fn generated_header(objects: usize, texts: usize) -> String {
    format!(
        "//! The realm as constant data: {objects} objects and their properties, read where they \
         lie.\n\
         //!\n\
         //! # GENERATED. DO NOT EDIT.\n\
         //!\n\
         //! Regenerated by the `realm_tables` test module with `{REWRITE_VARIABLE}` set, and \
         checked on\n\
         //! every build by `the_tables_build_the_realm_that_install_builds`, which assembles an \
         object\n\
         //! from every descriptor here and compares it field by field against the one the \
         installers\n\
         //! produce. **An edit made here rather than to the installers is reverted by the next \
         person\n\
         //! who regenerates, and reported by that test in the meantime.**\n\
         //!\n\
         //! # WHY THE FUNCTION BODIES ARE NOT HERE\n\
         //!\n\
         //! A callable descriptor carries `Callable::Native(u32)`, which is an INDEX into the \
         registry\n\
         //! the installers build at start-up -- not a function pointer. A generated table cannot \
         name a\n\
         //! built-in's body, because the bodies are items inside the installer module that no \
         outside\n\
         //! path reaches. **So these indices mean whatever the registration order says they \
         mean**, and\n\
         //! that order is what the committed realm rendering exists to hold still.\n\
         //!\n\
         //! The text below is interned: {texts} distinct strings stand in for every string key and \
         string\n\
         //! value in the realm. They are `static` rather than `const` so that each has exactly one\n\
         //! address.\n\
         \n\
         use crate::object::{{Callable, Property, PropertyKey, PropertyKind, ResidentObject}};\n\
         use crate::string_value::JsString;\n\
         use crate::value::{{JsValue, ObjectId, SymbolId}};\n"
    )
}

fn optional_id_source(id: Option<ObjectId>) -> String {
    match id {
        Some(ObjectId(value)) => format!("Some(ObjectId({value}))"),
        None => String::from("None"),
    }
}

/// A callable slot as Rust, and **`Native` is the only variant it will emit.**
///
/// # EVERY OTHER VARIANT IS AN INDEX INTO A TABLE NOTHING HOLDS STILL
///
/// All 414 callable realm objects are `Native` today, and the registry's order is exactly what the
/// realm rendering exists to pin. The other index-carrying variants have no such instrument:
/// `Closure` indexes the closure table and `Bound` the bound-function table, both built while a
/// program runs and neither recorded anywhere a drift gate can read. **An emitted `Closure(7)`
/// would therefore mean whatever the seventh closure happened to be**, which is the same silent
/// failure the native ordering was guarded against, with nothing guarding it.
///
/// The remaining five carry an `ObjectId` into state a running program owns -- a promise, a
/// combinator's results, a proxy pair -- so a realm built before any program has run cannot hold
/// one at all, and a plausible row would put a dangling id in flash.
///
/// So this refuses, loudly and by name, rather than rendering something that would compile. A
/// built-in acquiring any of them is a decision to take, not a row to generate.
fn callable_source(callable: Option<Callable>) -> String {
    match callable {
        None => String::from("None"),
        Some(Callable::Native(index)) => format!("Some(Callable::Native({index}))"),
        Some(other) => panic!(
            "a realm object is callable as {other:?}, and only `Native` carries an index whose \
             meaning is held still -- emitting this one would bake in a number that means whatever \
             the run-time table happened to hold"
        ),
    }
}

fn key_source(pool: &[Text], key: &PropertyKey) -> String {
    match key {
        PropertyKey::String(text) => {
            format!("PropertyKey::from_static_units(&{})", interned(pool, text.units()))
        }
        PropertyKey::Symbol(id) => format!("PropertyKey::Symbol(SymbolId({}))", id.0),
    }
}

fn value_source(pool: &[Text], value: &JsValue) -> String {
    match value {
        JsValue::Undefined => String::from("JsValue::Undefined"),
        JsValue::Null => String::from("JsValue::Null"),
        JsValue::Boolean(value) => format!("JsValue::Boolean({value})"),
        JsValue::Number(value) => format!("JsValue::Number({})", number_source(*value)),
        JsValue::String(text) => format!(
            "JsValue::String(JsString::from_static_units(&{}))",
            interned(pool, text.units())
        ),
        JsValue::Symbol(id) => format!("JsValue::Symbol(SymbolId({}))", id.0),
        JsValue::Object(id) => format!("JsValue::Object(ObjectId({}))", id.0),
    }
}

/// A property as a STRUCT LITERAL, every field named.
///
/// # THE THREE NAMED CONSTRUCTORS WOULD NOT COVER IT, AND THAT IS THE SMALLER REASON
///
/// Five attribute combinations occur -- 855 non-writable non-enumerable configurable, 436 built-in,
/// 90 wholly frozen, 16 ordinary, and a single writable non-configurable one -- against three
/// constructors, so two more would have to be added and named. The larger reason is the one
/// `ResidentObject` gives for being a struct: a positional call is a row of `false, false, true`
/// with the meaning carried by position, which is the shape that makes a misgenerated field
/// invisible when somebody reviews the diff.
fn property_source(pool: &[Text], property: &Property) -> String {
    let kind = match &property.kind {
        PropertyKind::Data { value, writable } => format!(
            "PropertyKind::Data {{ value: {}, writable: {writable} }}",
            value_source(pool, value)
        ),
        PropertyKind::Accessor { get, set } => format!(
            "PropertyKind::Accessor {{ get: {}, set: {} }}",
            optional_id_source(*get),
            optional_id_source(*set)
        ),
    };
    format!(
        "Property {{ kind: {kind}, enumerable: {}, configurable: {} }}",
        property.enumerable, property.configurable
    )
}

/// A number as a Rust literal that reads back to the same bits.
///
/// The finite case reuses [`number`]'s round-trip check, so the two renderings cannot disagree
/// about a value. The three non-finite ones need Rust's own names, since no literal denotes them --
/// and `f64::NAN` is the one place the emitter is deliberately lossy, for the reason [`number`]
/// gives.
fn number_source(value: f64) -> String {
    match number(value).as_str() {
        "nan" => String::from("f64::NAN"),
        "inf" => String::from("f64::INFINITY"),
        "-inf" => String::from("f64::NEG_INFINITY"),
        finite => {
            assert!(
                finite.contains('.') || finite.contains('e') || finite.contains('E'),
                "`{finite}` is not spelled as a float literal"
            );
            String::from(finite)
        }
    }
}

/// A number rendered so that reading it back gives the same bits.
///
/// # THE ROUND TRIP IS CHECKED RATHER THAN TRUSTED
///
/// A property value is an `f64` and the realm holds 452 of them, including `Number.EPSILON`,
/// `Number.MAX_VALUE` and `Number.MIN_VALUE`. A rendering that is one unit in the last place away
/// from the value it describes is a wrong answer that looks exactly like a right one, so every
/// finite number is parsed back and compared BY BITS before it is emitted.
///
/// The three non-finite values are spelled out because no decimal literal denotes them. NaN is
/// rendered without its payload, which is a real loss and not an observable one: the language has
/// a single NaN, and no program can tell two payloads apart.
fn number(value: f64) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value == f64::INFINITY {
        return "inf".into();
    }
    if value == f64::NEG_INFINITY {
        return "-inf".into();
    }
    let rendered = format!("{value:?}");
    let parsed: f64 = rendered
        .parse()
        .unwrap_or_else(|_| panic!("`{rendered}` does not read back as a number at all"));
    assert_eq!(
        parsed.to_bits(),
        value.to_bits(),
        "`{rendered}` reads back as a different number than the one it was rendered from"
    );
    rendered
}

/// UTF-16 text as a quoted, fully reversible literal.
///
/// Printable ASCII is written as itself and everything else as `\uXXXX`. Rendering through a
/// lossy conversion instead would collapse an unpaired surrogate onto the replacement character,
/// and two distinct property keys would render identically.
fn quoted(units: &[u16]) -> String {
    let mut out = String::with_capacity(units.len() + 2);
    out.push('"');
    for unit in units {
        match unit {
            0x22 => out.push_str("\\\""),
            0x5C => out.push_str("\\\\"),
            0x20..=0x7E => out.push(char::from(*unit as u8)),
            other => out.push_str(&format!("\\u{other:04x}")),
        }
    }
    out.push('"');
    out
}

/// The same rendering for a Rust string, so a native's name and a property key escape alike.
fn quoted_str(text: &str) -> String {
    quoted(&text.encode_utf16().collect::<Vec<u16>>())
}

/// The first place two renderings differ, located, or `None` when they agree.
///
/// # IT REPORTS A PLACE, NOT A VERDICT
///
/// The failure this gate exists for -- a native registered in the middle of the file -- moves one
/// line and then every line after it, so "they differ" is useless and "they first differ at line
/// N, which is `nat 214`" names the edit. The count of differing lines is reported beside it
/// because a single moved registration and a wholesale regeneration look identical at the first
/// line and nothing alike in the total.
///
/// Trailing carriage returns are stripped from both sides before comparing. The committed copy is
/// stored with newline endings, and a checkout that converted them would otherwise report every
/// line as changed and hide whatever really moved.
fn first_difference(committed: &str, produced: &str) -> Option<String> {
    let left: Vec<&str> = committed.lines().map(|line| line.trim_end_matches('\r')).collect();
    let right: Vec<&str> = produced.lines().map(|line| line.trim_end_matches('\r')).collect();
    let differing = left
        .iter()
        .zip(right.iter())
        .filter(|(a, b)| a != b)
        .count()
        + left.len().abs_diff(right.len());
    if differing == 0 {
        return None;
    }
    let at = left.iter().zip(right.iter()).position(|(a, b)| a != b).unwrap_or(left.len().min(right.len()));
    let committed_line = left.get(at).copied().unwrap_or("<end of the committed copy>");
    let produced_line = right.get(at).copied().unwrap_or("<end of the produced copy>");
    Some(format!(
        "the installed realm no longer matches the committed rendering\n  \
         {differing} of {} line(s) differ; the first is line {}\n  \
         committed: {committed_line}\n  \
         installed: {produced_line}\n  \
         If the change is intended, regenerate with {REWRITE_VARIABLE}=1 and READ THE DIFF: a \
         moved native registration renumbers every later index, and each renumbered index is a \
         built-in that will run the wrong body.",
        left.len().max(right.len()),
        at + 1,
    ))
}

#[cfg(test)]
mod the_realm_does_not_drift_under_its_tables {
    use super::*;

    /// THE GATE. It compares by default; the rewrite arm is the exception and it fails.
    #[test]
    fn the_committed_rendering_still_describes_the_installed_realm() {
        let produced = snapshot(&Interpreter::with_installed_realm());

        if std::env::var_os(REWRITE_VARIABLE).is_some() {
            std::fs::write(SNAPSHOT_PATH, &produced)
                .unwrap_or_else(|error| panic!("could not write {SNAPSHOT_PATH}: {error}"));
            std::fs::write(TABLES_PATH, generate(&Interpreter::with_installed_realm()))
                .unwrap_or_else(|error| panic!("could not write {TABLES_PATH}: {error}"));
            panic!(
                "{SNAPSHOT_PATH} and {TABLES_PATH} were REWRITTEN from the installed realm, not \
                 compared against it. Read the diff, then re-run without {REWRITE_VARIABLE} to \
                 check it."
            );
        }

        let committed = std::fs::read_to_string(SNAPSHOT_PATH).unwrap_or_else(|error| {
            panic!(
                "could not read {SNAPSHOT_PATH} ({error}); a test binary runs with its package \
                 root as the working directory, so the path is relative to \
                 crates/lamella-js-frontend"
            )
        });
        if let Some(report) = first_difference(&committed, &produced) {
            panic!("{report}");
        }
    }

    /// The rendering has to be a function of the realm and of nothing else, or the gate reports a
    /// difference on every second run and gets turned off.
    ///
    /// Both arms, because they are two different constructions: one runs the installers and one
    /// reads the tables, and either could pick up something a second run would not reproduce.
    #[test]
    fn two_installations_render_identically() {
        let installed = || snapshot(&Interpreter::with_installed_realm());
        let shipping = || snapshot(&Interpreter::new());
        assert_eq!(installed(), installed());
        assert_eq!(shipping(), shipping());
        assert_eq!(
            generate(&Interpreter::with_installed_realm()),
            generate(&Interpreter::with_installed_realm())
        );
    }

    /// THE INSTALLER SPLIT'S ACCEPTANCE TEST: the realm a shipping build gets, rendered whole,
    /// against the realm the installers build, rendered whole.
    ///
    /// # IT COVERS THE THREE THINGS THE SPLIT COULD BREAK, AND THEY ARE NOT THE OBVIOUS ONE
    ///
    /// The objects agreeing matters least, because in a shipping build they came from tables that
    /// were generated from this same oracle. What the split genuinely risks is everything the
    /// installers still do while creating nothing:
    ///
    /// - **the native registry**, whose order is the meaning of every `Callable::Native(u32)` the
    ///   tables carry -- an installer that registers a different set while replaying repoints them;
    /// - **the `Intrinsics` ids**, which are captured from the ids `allocate` hands back, so a
    ///   single extra or missing object silently renames every intrinsic after it;
    /// - **the symbol table**, on whose order forty property keys depend.
    ///
    /// All three are sections of this rendering, and all three come from running code in both arms.
    /// A whole-text comparison covers them together rather than as three checks somebody remembered
    /// to write.
    #[test]
    fn the_shipping_realm_is_the_realm_the_installers_build() {
        let installed = snapshot(&Interpreter::with_installed_realm());
        let shipping = snapshot(&Interpreter::new());
        if let Some(report) = first_difference(&installed, &shipping) {
            panic!("the installer split changed the realm\n{report}");
        }
    }

    /// The split has to have actually happened, which a rendering comparison cannot show: two
    /// realms with identical CONTENTS is exactly what it asserts, and it would still hold if the
    /// installers were quietly building everything a second time.
    ///
    /// So this measures the thing that is supposed to have gone away. A shipping realm holds no
    /// property storage at all, and the one the installers build holds all of it.
    #[test]
    fn the_shipping_realm_pays_for_no_property_storage_and_the_built_one_pays_for_all_of_it() {
        let shipping = Interpreter::new().realm_census();
        let installed = Interpreter::with_installed_realm().realm_census();

        assert_eq!(shipping.objects, installed.objects, "the two realms differ in size");
        assert_eq!(
            shipping.property_store_bytes, 0,
            "a table-built realm allocated {} bytes of property storage",
            shipping.property_store_bytes
        );
        assert!(
            installed.property_store_bytes > 100_000,
            "the installers' realm holds only {} bytes -- the comparison has lost its subject",
            installed.property_store_bytes
        );
        assert!(shipping.object_struct_bytes > 0, "the object structs stopped costing anything");
        assert_eq!(
            shipping.object_struct_bytes, installed.object_struct_bytes,
            "the two realms disagree about what their structs cost"
        );
    }

    /// THE EMITTER'S ACCEPTANCE TEST, AND IT COMPARES OBJECTS RATHER THAN TEXT.
    ///
    /// A text comparison of the generated file against a freshly generated one would only ever
    /// answer *did somebody forget to regenerate*. This answers the question that matters: **does
    /// an object assembled from the committed descriptor equal the one the installers build**, for
    /// all 462, field by field, property by property, over exactly the fields the realm rendering
    /// covers.
    ///
    /// It is the strictly stronger check and it subsumes the weaker one: an installer edit that
    /// was never regenerated into the tables fails here, because the tables then describe the old
    /// realm. It also catches what a text comparison cannot -- a generator that renders a field
    /// wrongly renders it wrongly on both sides of a text diff and identically every time.
    #[test]
    fn the_tables_build_the_realm_that_install_builds() {
        let interpreter = Interpreter::with_installed_realm();
        let objects = interpreter.realm_census().objects;
        assert_eq!(
            crate::realm::REALM.len(),
            objects,
            "the committed tables describe {} objects and the installers built {objects}",
            crate::realm::REALM.len()
        );
        for (id, descriptor) in crate::realm::REALM.iter().enumerate() {
            let installed = object_text(id, interpreter.object(ObjectId(id as u32)));
            let assembled = object_text(id, &Object::resident(descriptor));
            assert_eq!(
                installed, assembled,
                "the descriptor for object {id} does not build the object the installers build"
            );
        }
    }

    /// The interned text is what the tables save, so the saving is asserted rather than described.
    ///
    /// Interning is the emitter's one non-obvious decision, and a generator that quietly stopped
    /// deduplicating would still produce a correct realm -- it would simply cost four times the
    /// flash, and nothing else in the suite would notice.
    #[test]
    fn the_text_pool_is_far_smaller_than_the_references_into_it() {
        let interpreter = Interpreter::with_installed_realm();
        let pool = intern(&interpreter);
        let mut references = 0usize;
        for id in 0..interpreter.realm_census().objects {
            let object = interpreter.object(ObjectId(id as u32));
            for (key, property) in object.named_entries() {
                if matches!(key, PropertyKey::String(_)) {
                    references += 1;
                }
                if matches!(property.data_value(), Some(JsValue::String(_))) {
                    references += 1;
                }
            }
        }
        assert!(references > 1_800, "only {references} string references -- the walk missed some");
        assert!(
            pool.len() * 4 < references,
            "{} interned strings against {references} references: the pool stopped deduplicating",
            pool.len()
        );
        let mut names: Vec<&str> = pool.iter().map(|entry| entry.name.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "two interned strings share a static name");
    }

    /// The generated source has to be the whole realm, and a section that came out empty would
    /// otherwise read as a realm that has nothing of that kind in it.
    #[test]
    fn the_generated_source_carries_every_descriptor() {
        let interpreter = Interpreter::with_installed_realm();
        let source = generate(&interpreter);
        let objects = interpreter.realm_census().objects;
        assert_eq!(
            source.matches("    ResidentObject { prototype:").count(),
            objects,
            "the generated table does not carry {objects} descriptors"
        );
        assert!(source.contains(&format!("static REALM: [ResidentObject; {objects}]")));
        let dense: usize = (0..objects)
            .map(|id| interpreter.object(ObjectId(id as u32)).element_slots().len())
            .sum();
        assert_eq!(dense, 0, "the realm has {dense} dense elements now; the emitter emits them");
        assert_eq!(source.matches("static ELEMENTS_").count(), 0);
    }

    /// THE GATE PROVED ON THE FAILURE IT EXISTS FOR, RATHER THAN ASSUMED.
    ///
    /// A guard that has never fired is a guard nobody has tested, and a comparison is the easiest
    /// thing in the world to write so that it can only pass. This takes the real rendering,
    /// performs the exact edit the gate is aimed at -- a native registered one earlier, so every
    /// later index shifts by one -- and asserts that the comparison notices and points at it.
    #[test]
    fn a_native_registered_in_the_middle_is_reported_and_located() {
        let real = snapshot(&Interpreter::with_installed_realm());
        let mut lines: Vec<String> = real.lines().map(String::from).collect();

        let first_native = lines
            .iter()
            .position(|line| line.starts_with("nat "))
            .expect("the rendering carries a native registry");
        let inserted_at = first_native + 200;
        lines.insert(inserted_at, "nat 200 name \"interloper\" length 0".into());

        let perturbed = lines.join("\n") + "\n";
        let report = first_difference(&real, &perturbed).expect("an inserted native went unnoticed");
        assert!(
            report.contains(&format!("the first is line {}", inserted_at + 1)),
            "the report did not locate the insertion: {report}"
        );
        assert!(report.contains("interloper"), "the report did not show the offending line");

        assert!(first_difference(&real, &real).is_none(), "a rendering differed from itself");
    }

    /// A shorter or longer rendering is a difference even when every shared line agrees -- an
    /// object or a native DELETED off the end is exactly that shape.
    #[test]
    fn a_truncated_rendering_is_a_difference() {
        let real = snapshot(&Interpreter::with_installed_realm());
        let truncated: String = real.lines().rev().skip(1).collect::<crate::Vec<_>>()
            .into_iter().rev().collect::<crate::Vec<_>>().join("\n");
        let report = first_difference(&real, &truncated).expect("a truncation went unnoticed");
        assert!(report.contains("line(s) differ"), "{report}");
    }

    /// Every number in the realm survives the round trip, checked here as well as at render time
    /// so the failure is a named test rather than a panic inside the serializer.
    #[test]
    fn every_number_in_the_realm_renders_back_to_its_own_bits() {
        let interpreter = Interpreter::with_installed_realm();
        let mut checked = 0usize;
        let mut non_finite = 0usize;
        for id in 0..interpreter.realm_census().objects {
            let object = interpreter.object(ObjectId(id as u32));
            let values = object
                .named_entries()
                .iter()
                .filter_map(|(_, property)| property.data_value())
                .chain(object.element_slots().iter().flatten().filter_map(Property::data_value))
                .chain(object.primitive.iter());
            for value in values {
                let JsValue::Number(number_value) = value else { continue };
                checked += 1;
                if !number_value.is_finite() {
                    non_finite += 1;
                    continue;
                }
                let rendered = number(*number_value);
                let parsed: f64 = rendered.parse().expect("a rendered number reads back");
                assert_eq!(parsed.to_bits(), number_value.to_bits(), "`{rendered}` lost a bit");
            }
        }
        assert!(checked > 400, "only {checked} numbers -- the walk missed the realm");
        assert!(non_finite >= 3, "only {non_finite} non-finite numbers; the special arms are untried");
    }

    /// Escaping is reversible for every key text the realm actually contains, and the check is
    /// that two distinct keys never render alike.
    #[test]
    fn no_two_realm_keys_render_to_the_same_text() {
        let interpreter = Interpreter::with_installed_realm();
        for id in 0..interpreter.realm_census().objects {
            let object = interpreter.object(ObjectId(id as u32));
            let mut seen: Vec<String> = Vec::new();
            for (key, _) in object.named_entries() {
                let text = key_text(key);
                assert!(
                    !seen.contains(&text),
                    "object {id} has two properties rendering as `{text}`"
                );
                seen.push(text);
            }
        }
    }

    /// A non-ASCII and an unpaired-surrogate key both render reversibly, which the realm's own
    /// keys cannot demonstrate because all of them are ASCII.
    #[test]
    fn escaping_survives_text_the_realm_does_not_contain() {
        assert_eq!(quoted(&[0x41, 0x42]), "\"AB\"");
        assert_eq!(quoted(&[0x22, 0x5C]), "\"\\\"\\\\\"");
        assert_eq!(quoted(&[0x00E9]), "\"\\u00e9\"");
        assert_eq!(quoted(&[0xD83D]), "\"\\ud83d\"");
        assert_ne!(quoted(&[0xD83D]), quoted(&[0xDE00]));
        assert_eq!(quoted(&[0x0009, 0x000A]), "\"\\u0009\\u000a\"");
    }

    /// THE CONTRACT FORTY OF THE REALM'S PROPERTY KEYS DEPEND ON.
    ///
    /// A symbol key is `PropertyKey::Symbol(SymbolId(n))`, and `SymbolId` is an INDEX into the
    /// realm's symbol table. The well-known symbols are the first twelve entries because
    /// `install_symbol` allocates them before anything else in the realm allocates a symbol --
    /// which is true, and was true only by arrangement. **Anything allocating a symbol earlier
    /// shifts all twelve**, and every symbol-keyed property in the realm would then be filed under
    /// a valid, wrong key: still a key, still readable, and reachable by nothing.
    ///
    /// That is why the identity is asserted rather than described. The corresponding fact for a
    /// generated table is that a descriptor emits the NUMBER, so the number has to keep meaning
    /// what it meant when the table was written.
    #[test]
    fn the_twelve_well_known_symbols_are_the_first_twelve_symbol_ids() {
        use crate::interpreter::WELL_KNOWN_SYMBOLS;
        let interpreter = Interpreter::with_installed_realm();
        for (index, name) in WELL_KNOWN_SYMBOLS.iter().enumerate() {
            assert_eq!(
                interpreter.intrinsics.well_known_symbols[index].0 as usize,
                index,
                "`Symbol.{name}` is not `SymbolId({index})` -- something allocated a symbol first, \
                 and every symbol-keyed property in the realm has moved with it"
            );
        }
        assert_eq!(
            interpreter.symbol_table().len(),
            WELL_KNOWN_SYMBOLS.len(),
            "a fresh realm holds symbols beyond the well-known ones; the first twelve are still \
             pinned above, but the table is no longer only them"
        );
    }

    /// The rendering has to be TOTAL over the realm, so a section that silently came out empty
    /// would be caught here rather than by a reviewer noticing an absence.
    #[test]
    fn the_rendering_carries_every_section_and_every_object() {
        let interpreter = Interpreter::with_installed_realm();
        let text = snapshot(&interpreter);
        for section in ["[symbols]", "[natives]", "[intrinsics]", "[objects]"] {
            assert!(text.contains(section), "`{section}` is missing from the rendering");
        }
        let objects = interpreter.realm_census().objects;
        for id in [0, objects / 2, objects - 1] {
            assert!(
                text.contains(&format!("\nobj {id} proto ")),
                "object {id} has no header line in the rendering"
            );
        }
        let headers = text.lines().filter(|line| line.contains(" proto ")).count();
        assert_eq!(headers, objects, "the rendering describes {headers} of {objects} objects");
    }
}
