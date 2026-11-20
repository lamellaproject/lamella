//! Bounded structural rendering shared by native and interpreter-aware string conversions.

use alloc::string::String;
use alloc::vec::Vec;

use crate::object::{DictViewKind, ObjectModel};
use crate::{Trap, Value};

/// The representation to assemble after its child values have been rendered.
pub(crate) enum ReprForm {
    Exception,
    NamedTuple(Vec<String>),
    Sequence(bool),
    Set(bool),
    Counter,
    Mapping,
    DefaultDict(String),
    Deque,
    DictView(DictViewKind),
    Slice,
    Bound(String),
    Literal(String),
    Group(usize),
}

/// A native format, its live metadata object, and its children in rendering order.
pub(crate) struct ReprChildren {
    pub form: ReprForm,
    pub metadata: Value,
    pub children: Vec<Value>,
}

/// Snapshots native children without calling Python or allocating in the object heap.
pub(crate) fn repr_children(value: Value, model: &ObjectModel) -> Option<ReprChildren> {
    let (form, metadata, children) = if let Some(args) = model.exception_args(value) {
        (ReprForm::Exception, value, args.to_vec())
    } else if let Some(class) = model.ntinstance_class(value) {
        let fields = model.ntclass_fields(class);
        let mut elems = model.seq_value(value).cloned().unwrap_or_default();
        elems.truncate(fields.len());
        (ReprForm::NamedTuple(fields), class, elems)
    } else if let Some(elems) = model.seq_value(value) {
        (
            ReprForm::Sequence(model.is_tuple(value)),
            value,
            elems.clone(),
        )
    } else if let Some(elems) = model.set_value(value) {
        (
            ReprForm::Set(model.is_frozenset(value)),
            value,
            elems.clone(),
        )
    } else if let Some(entries) = model.dict_value(value) {
        let entries = entries.clone();
        let (form, entries) = if model.is_counter(value) {
            (ReprForm::Counter, model.counter_display_entries(entries))
        } else {
            (ReprForm::Mapping, entries)
        };
        (
            form,
            value,
            entries
                .into_iter()
                .flat_map(|(key, val)| [key, val])
                .collect(),
        )
    } else if let Some(elems) = model.deque_elems(value) {
        (ReprForm::Deque, value, elems.iter().copied().collect())
    } else if let Some(kind) = model.dict_view_kind(value) {
        let entries = model
            .dict_value(model.dict_view_dict(value))
            .cloned()
            .unwrap_or_default();
        let elems = match kind {
            DictViewKind::Keys => entries.into_iter().map(|(key, _)| key).collect(),
            DictViewKind::Values => entries.into_iter().map(|(_, val)| val).collect(),
            DictViewKind::Items => entries
                .into_iter()
                .flat_map(|(key, val)| [key, val])
                .collect(),
        };
        (ReprForm::DictView(kind), value, elems)
    } else if model.is_slice(value) {
        let (start, stop, step) = model.slice_components(value);
        (ReprForm::Slice, value, alloc::vec![start, stop, step])
    } else if model.is_py_bound(value) {
        let name = model
            .function_qualname(model.bound_func(value))
            .unwrap_or_default();
        (
            ReprForm::Bound(name),
            value,
            alloc::vec![model.bound_self(value)],
        )
    } else {
        return None;
    };
    Some(ReprChildren {
        form,
        metadata,
        children,
    })
}

/// The recursive form of a container. Exceptions, slices, and bound methods use the depth limit.
pub(crate) fn repr_cycle(value: Value, model: &ObjectModel) -> Option<ReprChildren> {
    let text = if model.ntinstance_class(value).is_some() || model.is_counter(value) {
        return None;
    } else if model.is_list(value) || model.deque_elems(value).is_some() {
        "[...]"
    } else if model.is_tuple(value) {
        "(...)"
    } else if model.is_ordereddict(value) || model.is_dict_view(value) {
        "..."
    } else if let Some(factory) = model.defaultdict_factory(value) {
        return Some(ReprChildren {
            form: ReprForm::DefaultDict(String::from("...")),
            metadata: value,
            children: alloc::vec![factory],
        });
    } else if model.dict_value(value).is_some() {
        "{...}"
    } else if model.set_value(value).is_some() {
        if model.is_frozenset(value) {
            "frozenset(...)"
        } else {
            "set(...)"
        }
    } else {
        return None;
    };
    Some(ReprChildren {
        form: ReprForm::Literal(String::from(text)),
        metadata: value,
        children: Vec::new(),
    })
}

/// A mapping's contents, shared with its delayed default-factory rendering.
pub(crate) fn mapping_body(parts: &[String]) -> String {
    parts
        .chunks_exact(2)
        .map(|pair| alloc::format!("{}: {}", pair[0], pair[1]))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Joins already-rendered children; it never recurses or calls Python.
pub(crate) fn repr_join(
    form: ReprForm,
    metadata: Value,
    parts: Vec<String>,
    model: &ObjectModel,
) -> String {
    match form {
        ReprForm::Exception => {
            let name = model
                .exception_type_name(metadata)
                .unwrap_or("BaseException");
            alloc::format!("{name}({})", parts.join(", "))
        }
        ReprForm::NamedTuple(fields) => {
            let parts: Vec<_> = fields
                .iter()
                .zip(parts)
                .map(|(field, text)| alloc::format!("{field}={text}"))
                .collect();
            alloc::format!("{}({})", model.ntclass_name(metadata), parts.join(", "))
        }
        ReprForm::Sequence(tuple) => {
            let inner = parts.join(", ");
            if tuple {
                if parts.len() == 1 {
                    alloc::format!("({inner},)")
                } else {
                    alloc::format!("({inner})")
                }
            } else {
                alloc::format!("[{inner}]")
            }
        }
        ReprForm::Set(frozen) => {
            if parts.is_empty() {
                String::from(if frozen { "frozenset()" } else { "set()" })
            } else {
                let inner = parts.join(", ");
                if frozen {
                    alloc::format!("frozenset({{{inner}}})")
                } else {
                    alloc::format!("{{{inner}}}")
                }
            }
        }
        ReprForm::Counter | ReprForm::Mapping => {
            let inner = mapping_body(&parts);
            if matches!(form, ReprForm::Counter) {
                if parts.is_empty() {
                    String::from("Counter()")
                } else {
                    alloc::format!("Counter({{{inner}}})")
                }
            } else if model.is_ordereddict(metadata) {
                if parts.is_empty() {
                    String::from("OrderedDict()")
                } else {
                    alloc::format!("OrderedDict({{{inner}}})")
                }
            } else {
                alloc::format!("{{{inner}}}")
            }
        }
        ReprForm::DefaultDict(inner) => alloc::format!("defaultdict({}, {{{inner}}})", parts[0]),
        ReprForm::Deque => {
            let inner = parts.join(", ");
            match model.deque_maxlen(metadata).unwrap_or(None) {
                Some(m) => alloc::format!("deque([{inner}], maxlen={m})"),
                None => alloc::format!("deque([{inner}])"),
            }
        }
        ReprForm::DictView(kind) => {
            let (name, parts) = match kind {
                DictViewKind::Keys => ("dict_keys", parts),
                DictViewKind::Values => ("dict_values", parts),
                DictViewKind::Items => (
                    "dict_items",
                    parts
                        .chunks_exact(2)
                        .map(|pair| alloc::format!("({}, {})", pair[0], pair[1]))
                        .collect(),
                ),
            };
            alloc::format!("{name}([{}])", parts.join(", "))
        }
        ReprForm::Slice => alloc::format!("slice({})", parts.join(", ")),
        ReprForm::Bound(name) => alloc::format!("<bound method {name} of {}>", parts[0]),
        ReprForm::Literal(text) => text,
        ReprForm::Group(count) => {
            let plural = if count == 1 { "" } else { "s" };
            alloc::format!("{} ({count} sub-exception{plural})", parts[0])
        }
    }
}

/// Renders without user methods, retaining active containers until their children return.
/// Each child uses the interpreter's native stack budget.
/// See the Python 3.14 Library Reference, Built-in Exceptions (RecursionError).
#[inline(never)]
pub(crate) fn native_render(
    value: Value,
    display: bool,
    model: &ObjectModel,
) -> Result<String, Trap> {
    render_with_active(value, display, model, &mut Vec::new())
}

#[inline(never)]
fn render_with_active(
    value: Value,
    display: bool,
    model: &ObjectModel,
    active: &mut Vec<Value>,
) -> Result<String, Trap> {
    let _native_call = model.enter_native_call()?;
    let cycle = repr_cycle(value, model);
    let repeated = active.contains(&value) || model.is_rendering(value);
    if repeated {
        if let Some(ReprChildren {
            form: ReprForm::Literal(text),
            ..
        }) = &cycle
        {
            return Ok(text.clone());
        }
    }
    if display {
        if model.str_bytes(value).is_some() {
            return Ok(String::from(model.str_text(value)?));
        }
        if let Some(args) = model.exception_args(value) {
            if model.is_exception_group(value) {
                if let (Some(message), Some(members)) =
                    (model.group_message(value), model.group_members(value))
                {
                    let display =
                        !(model.str_bytes(message).is_some() && model.str_text(message).is_err());
                    let text = render_with_active(message, display, model, active)?;
                    return Ok(repr_join(
                        ReprForm::Group(members.len()),
                        value,
                        alloc::vec![text],
                        model,
                    ));
                }
            }
            return match args {
                [] => Ok(String::new()),
                [one] => {
                    let key_error = model
                        .exc_class_lookup("KeyError")
                        .is_some_and(|class| model.is_instance_of(value, class));
                    // Diagnostic messages retain the escaped spelling of a surrogate.
                    let display = !key_error
                        && !(model.str_bytes(*one).is_some() && model.str_text(*one).is_err());
                    render_with_active(*one, display, model, active)
                }
                _ => {
                    let parts = args
                        .iter()
                        .map(|&arg| render_with_active(arg, false, model, active))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(repr_join(ReprForm::Sequence(true), value, parts, model))
                }
            };
        }
    }
    let tracked = cycle.is_some() && !repeated;
    let plan = if repeated && cycle.is_some() {
        cycle
    } else {
        repr_children(value, model)
    };
    if tracked {
        active.push(value);
    }
    let result = (|| {
        let Some(ReprChildren {
            form,
            metadata,
            children,
        }) = plan
        else {
            return Ok(model.repr_leaf(value));
        };
        let parts = children
            .into_iter()
            .map(|child| render_with_active(child, false, model, active))
            .collect::<Result<Vec<_>, _>>()?;
        if matches!(form, ReprForm::Mapping) {
            if let Some(factory) = model.defaultdict_factory(metadata) {
                let inner = mapping_body(&parts);
                let factory = render_with_active(factory, false, model, active)?;
                return Ok(repr_join(
                    ReprForm::DefaultDict(inner),
                    metadata,
                    alloc::vec![factory],
                    model,
                ));
            }
        }
        Ok(repr_join(form, metadata, parts, model))
    })();
    if tracked {
        active.pop();
    }
    result
}
