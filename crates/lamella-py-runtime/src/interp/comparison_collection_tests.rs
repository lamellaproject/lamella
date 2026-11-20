//! Pending comparisons retain their operands across compacting collection in a callback.

use super::*;

fn collecting_comparison(left: &str, right: &str, op: CmpOp) {
    collecting_projection(left, right, "", op);
}

fn collecting_projection(left: &str, right: &str, projection: &str, op: CmpOp) {
    let source = alloc::format!(
        r#"
from collections import deque, OrderedDict
class Equal:
    def __eq__(self, other):
        if not isinstance(other, Equal):
            return False
        if enabled:
            left.clear()
            right.clear()
        waste = ['moving' + str(i) for i in range(20)]
        return True
    def __hash__(self):
        return 1
garbage = [0] * 100
witness = ['alive']
enabled = False
left = {left}
right = {right}
a = left{projection}
b = right{projection}
enabled = True
garbage = None
"#
    );
    let bundle = lamella_py_frontend::compile_bundle("__main__", &source, &|_| None)
        .expect("compile collection comparison");
    let functions = bundle.entry.functions.clone();
    let mut model = ObjectModel::new(Vec::new(), 1024 * 1024);
    run_bundle(bundle, &mut model).expect("prepare comparison operands");
    let left = model.get_global("a").expect("left operand");
    let right = model.get_global("b").expect("right operand");
    let witness = model.get_global("witness").expect("collection witness");
    let before = model.collections();
    model.set_gc_stress(true);
    let result = compare_values(op, left, right, &functions, &mut model, 0);
    assert!(
        model.collections() > before,
        "comparison callback must collect"
    );
    assert_ne!(
        model.get_global("witness"),
        Some(witness),
        "a live object must move"
    );
    assert_eq!(result, Ok(Value::TRUE));
    eprintln!(
        "pending comparison collections: {}",
        model.collections() - before
    );
}

#[test]
fn item_view_comparison_retains_pending_pairs() {
    let items = "{'first': Equal(), 'second': ['pending' + ' item']}";
    collecting_projection(items, items, ".items()", CmpOp::Eq);
}

#[test]
fn key_view_comparison_reloads_the_needle_and_candidates() {
    let keys = "{Equal(): 0, 'last' + ' key': 1}";
    collecting_projection(keys, keys, ".keys()", CmpOp::Ge);
}

#[test]
fn pending_sequence_comparisons_collect() {
    collecting_comparison(
        "[[Equal(), 'pending' + ' item'], ['last' + ' item']]",
        "[[Equal(), 'pending' + ' item'], ['last' + ' item']]",
        CmpOp::Eq,
    );
}

#[test]
fn pending_deque_comparisons_collect() {
    collecting_comparison(
        "deque([Equal(), 'pending' + ' item'])",
        "deque([Equal(), 'pending' + ' item'])",
        CmpOp::Le,
    );
}

#[test]
fn pending_set_comparisons_collect() {
    collecting_comparison(
        "{Equal(), 'pending' + ' item'}",
        "{Equal(), 'pending' + ' item'}",
        CmpOp::Eq,
    );
}

#[test]
fn pending_dict_comparisons_collect() {
    collecting_comparison(
        "{'first': Equal(), 'second': ['pending' + ' item']}",
        "{'first': Equal(), 'second': ['pending' + ' item']}",
        CmpOp::Eq,
    );
}

#[test]
fn pending_ordereddict_comparisons_collect() {
    collecting_comparison(
        "OrderedDict([('first', Equal()), ('second', ['pending' + ' item'])])",
        "OrderedDict([('first', Equal()), ('second', ['pending' + ' item'])])",
        CmpOp::Eq,
    );
}
