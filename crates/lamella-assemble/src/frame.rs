//! The argument and local-variable slots of a method (ECMA-335 1st ed, III.1.5).

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::cell::RefCell;
use lamella_binder::{BoundStmt, BoundStmtKind, SpecialType, TypeSymbol};

/// Where a named variable lives in a method frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// An argument slot (`ldarg`/`starg`).
    Argument(u16),
    /// A local-variable slot (`ldloc`/`stloc`).
    Local(u16),
}

/// A method's variable slots, keyed by name, with the local types in slot order.
#[derive(Debug, Default)]
pub struct Frame {
    slots: BTreeMap<Box<str>, Slot>,
    /// The local-variable types in slot order. Behind a `RefCell` so a compiler
    /// temporary (e.g. spilling a value-type rvalue receiver) can be reserved during
    /// expression emission, which holds the frame only by shared reference.
    local_types: RefCell<Vec<TypeSymbol>>,
    /// The referent type of each byref (`ref`/`out`) parameter, by name. Such a
    /// parameter's argument slot holds an address: a read derefs it (`ldind`), a write
    /// stores through it (`stind`).
    byref_types: BTreeMap<Box<str>, TypeSymbol>,
    /// Local slots that must be PINNED in the signature -- a `fixed` statement's array
    /// holder, so the GC does not move the array while a pointer into it is live.
    pinned: RefCell<BTreeSet<u16>>,
}

impl Frame {
    /// A frame with no variables (for binding-free expressions).
    #[must_use]
    pub fn empty() -> Frame {
        Frame::default()
    }

    /// Builds the frame for a method: the parameters in order from `arg_base` (1
    /// for an instance method, whose argument 0 is `this`; 0 for a static method),
    /// then the locals the body declares.
    #[must_use]
    pub fn build(
        parameters: &[Box<str>],
        byref_params: &[(Box<str>, TypeSymbol)],
        body: &BoundStmt,
        arg_base: u16,
    ) -> Frame {
        let mut frame = Frame::default();
        for (index, name) in parameters.iter().enumerate() {
            frame
                .slots
                .insert(name.clone(), Slot::Argument(index as u16 + arg_base));
        }
        for (name, ty) in byref_params {
            frame.byref_types.insert(name.clone(), ty.clone());
        }
        frame.collect_locals(body);
        frame
    }

    /// The slot a name occupies, if any.
    #[must_use]
    pub fn slot(&self, name: &str) -> Option<Slot> {
        self.slots.get(name).copied()
    }

    /// The argument slot and referent type of `name` when it is a byref (`ref`/`out`)
    /// parameter, so a read derefs (`ldind`) and a write stores through it (`stind`).
    #[must_use]
    pub fn byref(&self, name: &str) -> Option<(u16, &TypeSymbol)> {
        let ty = self.byref_types.get(name)?;
        match self.slots.get(name)? {
            Slot::Argument(slot) => Some((*slot, ty)),
            Slot::Local(_) => None,
        }
    }

    /// The number of local-variable slots (the method's `.locals` count).
    #[must_use]
    pub fn local_count(&self) -> u16 {
        self.local_types.borrow().len() as u16
    }

    /// The local-variable types in slot order, for the local signature.
    #[must_use]
    pub fn local_types(&self) -> Vec<TypeSymbol> {
        self.local_types.borrow().clone()
    }

    /// The local-variable names in slot order, for debug info. (Parallel to
    /// [`Frame::local_types`].)
    #[must_use]
    pub fn local_names(&self) -> Vec<Box<str>> {
        let mut names = alloc::vec![Box::<str>::from(""); self.local_types.borrow().len()];
        for (name, slot) in &self.slots {
            if let Slot::Local(index) = slot {
                names[*index as usize] = name.clone();
            }
        }
        names
    }

    fn declare_local(&mut self, name: &str, ty: &TypeSymbol) {
        let slot = self.reserve_local(ty);
        self.slots.insert(name.into(), Slot::Local(slot));
    }

    /// Reserves an unnamed local of `ty` (a compiler temporary, such as the value a
    /// `return` inside a `try` parks before leaving, or a spilled value-type rvalue
    /// receiver), returning its slot index. Takes `&self` so emission can reserve a
    /// temporary while holding the frame by shared reference.
    pub fn reserve_local(&self, ty: &TypeSymbol) -> u16 {
        let mut locals = self.local_types.borrow_mut();
        let slot = locals.len() as u16;
        locals.push(ty.clone());
        slot
    }

    /// Reserves a PINNED local (a `fixed` array holder): the slot is reported by
    /// [`Frame::pinned_slots`] so its signature carries the `pinned` modifier.
    pub fn reserve_pinned_local(&self, ty: &TypeSymbol) -> u16 {
        let slot = self.reserve_local(ty);
        self.pinned.borrow_mut().insert(slot);
        slot
    }

    /// The local slots that must be `pinned` in the local-variable signature.
    #[must_use]
    pub fn pinned_slots(&self) -> BTreeSet<u16> {
        self.pinned.borrow().clone()
    }

    fn collect_locals(&mut self, stmt: &BoundStmt) {
        match &stmt.kind {
            BoundStmtKind::Local { ty, declarators } => {
                for declarator in declarators {
                    self.declare_local(&declarator.name, ty);
                }
            }
            BoundStmtKind::Block(statements) => {
                for statement in statements {
                    self.collect_locals(statement);
                }
            }
            BoundStmtKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.collect_locals(then_branch);
                if let Some(else_branch) = else_branch {
                    self.collect_locals(else_branch);
                }
            }
            BoundStmtKind::While { body, .. } | BoundStmtKind::DoWhile { body, .. } => {
                self.collect_locals(body);
            }
            BoundStmtKind::For {
                initializer, body, ..
            } => {
                for statement in initializer {
                    self.collect_locals(statement);
                }
                self.collect_locals(body);
            }
            BoundStmtKind::ForEach {
                name,
                element_type,
                body,
                ..
            } => {
                self.declare_local(name, element_type);
                self.collect_locals(body);
            }
            BoundStmtKind::Checked(inner)
            | BoundStmtKind::Unchecked(inner)
            | BoundStmtKind::Labeled { body: inner, .. } => self.collect_locals(inner),
            BoundStmtKind::Lock { body, .. } | BoundStmtKind::Using { body, .. } => {
                self.collect_locals(body);
            }
            BoundStmtKind::Fixed {
                name,
                element,
                body,
                ..
            } => {
                self.declare_local(name, &TypeSymbol::Pointer(Box::new(element.clone())));
                self.collect_locals(body);
            }
            BoundStmtKind::Try {
                body,
                catches,
                finally,
            } => {
                self.collect_locals(body);
                for catch in catches {
                    if let Some(name) = &catch.name {
                        let ty = catch
                            .exception_type
                            .clone()
                            .unwrap_or(TypeSymbol::Special(SpecialType::Object));
                        self.declare_local(name, &ty);
                    }
                    self.collect_locals(&catch.body);
                }
                if let Some(finally) = finally {
                    self.collect_locals(finally);
                }
            }
            BoundStmtKind::Switch { sections, .. } => {
                for section in sections {
                    for statement in &section.statements {
                        self.collect_locals(statement);
                    }
                }
            }
            _ => {}
        }
    }
}
