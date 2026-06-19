//! The argument and local-variable slots of a method (ECMA-335 1st ed, III.1.5).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
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
    local_types: Vec<TypeSymbol>,
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
    pub fn build(parameters: &[Box<str>], body: &BoundStmt, arg_base: u16) -> Frame {
        let mut frame = Frame::default();
        for (index, name) in parameters.iter().enumerate() {
            frame
                .slots
                .insert(name.clone(), Slot::Argument(index as u16 + arg_base));
        }
        frame.collect_locals(body);
        frame
    }

    /// The slot a name occupies, if any.
    #[must_use]
    pub fn slot(&self, name: &str) -> Option<Slot> {
        self.slots.get(name).copied()
    }

    /// The number of local-variable slots (the method's `.locals` count).
    #[must_use]
    pub fn local_count(&self) -> u16 {
        self.local_types.len() as u16
    }

    /// The local-variable types in slot order, for the local signature.
    #[must_use]
    pub fn local_types(&self) -> &[TypeSymbol] {
        &self.local_types
    }

    /// The local-variable names in slot order, for debug info. (Parallel to
    /// [`Frame::local_types`].)
    #[must_use]
    pub fn local_names(&self) -> Vec<Box<str>> {
        let mut names = alloc::vec![Box::<str>::from(""); self.local_types.len()];
        for (name, slot) in &self.slots {
            if let Slot::Local(index) = slot {
                names[*index as usize] = name.clone();
            }
        }
        names
    }

    fn declare_local(&mut self, name: &str, ty: &TypeSymbol) {
        let slot = Slot::Local(self.local_types.len() as u16);
        self.slots.insert(name.into(), slot);
        self.local_types.push(ty.clone());
    }

    /// Reserves an unnamed local of `ty` (a compiler temporary, such as the value a
    /// `return` inside a `try` parks before leaving), returning its slot index.
    pub fn reserve_local(&mut self, ty: &TypeSymbol) -> u16 {
        let slot = self.local_types.len() as u16;
        self.local_types.push(ty.clone());
        slot
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
            _ => {}
        }
    }
}
