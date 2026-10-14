//! The `_heapq` module: the ten heap operations behind `heapq` -- `heappush`, `heappop`,
//! `heapreplace`, `heappushpop`, `heapify`, and the five `_max` forms. `heapq.py` imports them and
//! keeps `nsmallest` and `nlargest` in Python.
//!
//! A min-heap is a `list` in which no element is less than its parent: the children of index k sit
//! at 2k+1 and 2k+2, so index 0 holds a smallest element. A max-heap mirrors it. These functions work
//! on the list's own storage, so a list's methods are never called. Every move is an exchange: a push
//! climbs the new element toward the root while it is better than its parent, and a re-seat carries
//! the element at its position down to a leaf through the better child (the right one when the two
//! are equal) before climbing it back. The only operation applied to an element is `<`, evaluated as
//! the `<` operator evaluates it. The list is read afresh at every step, and a comparison that changes
//! its length raises `RuntimeError`, leaving the list as it stands.

use alloc::format;
use alloc::string::String;

use lamella_py_bytecode::{CmpOp, Functions};

use crate::interp::{compare_values, py_truthy_dyn};
use crate::object::ObjectModel;
use crate::trap::Trap;
use crate::value::Value;

/// The five operations; each comes in a min form and a max form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeapOp {
    Push,
    Pop,
    Replace,
    PushPop,
    Heapify,
}

impl HeapOp {
    /// Whether the operation takes an item besides the heap.
    fn takes_item(self) -> bool {
        matches!(self, HeapOp::Push | HeapOp::Replace | HeapOp::PushPop)
    }
}

/// The `TypeError` for a keyword argument, which every one of the ten refuses before anything else.
pub(crate) fn keyword_refusal(name: &str, model: &mut ObjectModel) -> Trap {
    model.raise_named_exception("TypeError", &format!("_heapq.{name}() takes no keyword arguments"))
}

/// Calls `_heapq.<name>(*args)`: the argument count, then the type of the first argument, each with
/// CPython's message, then the operation.
pub(crate) fn call(
    op: HeapOp,
    is_max: bool,
    name: &str,
    args: &[Value],
    functions: &Functions,
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let wanted = if op.takes_item() { 2 } else { 1 };
    if args.len() != wanted {
        let message = if op.takes_item() {
            format!("{name} expected 2 arguments, got {}", args.len())
        } else {
            format!("_heapq.{name}() takes exactly one argument ({} given)", args.len())
        };
        return Err(model.raise_named_exception("TypeError", &message));
    }
    let list = args[0];
    if !model.is_list(list) {
        let type_name = if list.is_none() { String::from("None") } else { model.tp_name_of(list) };
        let which = if op.takes_item() { "argument 1" } else { "argument" };
        let message = format!("{name}() {which} must be list, not {type_name}");
        return Err(model.raise_named_exception("TypeError", &message));
    }
    let heap = Heap { list, is_max, functions, depth };
    match op {
        HeapOp::Push => heap.push(args[1], model).map(|()| Value::NONE),
        HeapOp::Pop => heap.pop(model),
        HeapOp::Replace => heap.replace(args[1], model),
        HeapOp::PushPop => heap.push_pop(args[1], model),
        HeapOp::Heapify => heap.heapify(model).map(|()| Value::NONE),
    }
}

/// A list being worked on as a heap of one family.
struct Heap<'f> {
    list: Value,
    is_max: bool,
    functions: &'f Functions,
    depth: usize,
}

impl Heap<'_> {
    fn len(&self, model: &ObjectModel) -> usize {
        model.seq_value(self.list).map_or(0, |items| items.len())
    }

    fn get(&self, model: &ObjectModel, index: usize) -> Result<Value, Trap> {
        model.seq_value(self.list).and_then(|items| items.get(index).copied()).ok_or(Trap::IndexError)
    }

    fn set(&self, model: &mut ObjectModel, index: usize, value: Value) -> Result<(), Trap> {
        let slot = model.list_items_mut(self.list).and_then(|items| items.get_mut(index));
        *slot.ok_or(Trap::IndexError)? = value;
        Ok(())
    }

    fn exchange(&self, model: &mut ObjectModel, a: usize, b: usize) -> Result<(), Trap> {
        let items = model.list_items_mut(self.list).ok_or(Trap::TypeError)?;
        if a.max(b) >= items.len() {
            return Err(Trap::IndexError);
        }
        items.swap(a, b);
        Ok(())
    }

    /// Whether `x` belongs above `y`: `x < y` in a min-heap, `y < x` in a max-heap, so the left
    /// operand is the one each family names first.
    fn better(&self, model: &mut ObjectModel, x: Value, y: Value) -> Result<bool, Trap> {
        let (left, right) = if self.is_max { (y, x) } else { (x, y) };
        let outcome = compare_values(CmpOp::Lt, left, right, self.functions, model, self.depth)?;
        py_truthy_dyn(outcome, self.functions, model, self.depth)
    }

    /// Raises the `RuntimeError` when a comparison changed the list's length.
    fn guard(&self, model: &mut ObjectModel, size: usize) -> Result<(), Trap> {
        if self.len(model) == size {
            return Ok(());
        }
        Err(model.raise_named_exception("RuntimeError", "list changed size during iteration"))
    }

    fn empty_error(model: &mut ObjectModel) -> Trap {
        model.raise_named_exception("IndexError", "index out of range")
    }

    /// Moves the element at `pos` up toward `start`, one exchange per step, while it is better than
    /// its parent. An element never passes a parent equal to it.
    fn climb(&self, model: &mut ObjectModel, start: usize, mut pos: usize) -> Result<(), Trap> {
        let size = self.len(model);
        while pos > start {
            let parent = (pos - 1) / 2;
            let (moving, above) = (self.get(model, pos)?, self.get(model, parent)?);
            let rises = self.better(model, moving, above)?;
            self.guard(model, size)?;
            if !rises {
                break;
            }
            self.exchange(model, pos, parent)?;
            pos = parent;
        }
        Ok(())
    }

    /// Re-seats the element at `pos`: carries it down to a leaf through the better child at every
    /// level, the right one unless the left is strictly better, then climbs it back.
    fn sink(&self, model: &mut ObjectModel, mut pos: usize) -> Result<(), Trap> {
        let size = self.len(model);
        let start = pos;
        let mut child = 2 * pos + 1;
        while child < size {
            let right = child + 1;
            if right < size {
                let (left_value, right_value) = (self.get(model, child)?, self.get(model, right)?);
                let left_better = self.better(model, left_value, right_value)?;
                self.guard(model, size)?;
                if !left_better {
                    child = right;
                }
            }
            self.exchange(model, pos, child)?;
            pos = child;
            child = 2 * pos + 1;
        }
        self.climb(model, start, pos)
    }

    fn push(&self, item: Value, model: &mut ObjectModel) -> Result<(), Trap> {
        model.list_append(self.list, item)?;
        let last = self.len(model) - 1;
        self.climb(model, 0, last)
    }

    fn pop(&self, model: &mut ObjectModel) -> Result<Value, Trap> {
        let last = model.list_items_mut(self.list).and_then(|items| items.pop());
        let Some(last) = last else { return Err(Self::empty_error(model)) };
        if self.len(model) == 0 {
            return Ok(last);
        }
        let first = self.get(model, 0)?;
        self.set(model, 0, last)?;
        self.sink(model, 0)?;
        Ok(first)
    }

    fn replace(&self, item: Value, model: &mut ObjectModel) -> Result<Value, Trap> {
        if self.len(model) == 0 {
            return Err(Self::empty_error(model));
        }
        let first = self.get(model, 0)?;
        self.set(model, 0, item)?;
        self.sink(model, 0)?;
        Ok(first)
    }

    /// Pushes `item` and pops the best, as one step: an `item` that the root is not better than is
    /// handed straight back, and the list is not touched.
    fn push_pop(&self, item: Value, model: &mut ObjectModel) -> Result<Value, Trap> {
        if self.len(model) == 0 {
            return Ok(item);
        }
        let root = self.get(model, 0)?;
        if !self.better(model, root, item)? {
            return Ok(item);
        }
        self.replace(item, model)
    }

    fn heapify(&self, model: &mut ObjectModel) -> Result<(), Trap> {
        for pos in (0..self.len(model) / 2).rev() {
            self.sink(model, pos)?;
        }
        Ok(())
    }
}
