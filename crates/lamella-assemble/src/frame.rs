//! The argument and local-variable slots of a method (ECMA-335 1st ed, III.1.5).

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::cell::RefCell;
use lamella_binder::{BoundStmt, BoundStmtKind, SpecialType, TypeSymbol};
use lamella_cil::{Instruction, Opcode, Operand};
use lamella_syntax::span::Span;

/// Where a named variable lives in a method frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// An argument slot (`ldarg`/`starg`).
    Argument(u16),
    /// A local-variable slot (`ldloc`/`stloc`).
    Local(u16),
}

impl Slot {
    /// The instruction that pushes this slot's CONTENTS. For a byref slot -- a `ref`/`out`
    /// parameter or a `ref` local -- that is the managed pointer it holds, which the caller then
    /// derefs.
    #[must_use]
    pub fn load(self) -> Instruction {
        match self {
            Slot::Argument(index) => Instruction::new(Opcode::Ldarg, Operand::Variable(index)),
            Slot::Local(index) => Instruction::new(Opcode::Ldloc, Operand::Variable(index)),
        }
    }

    /// The instruction that overwrites this slot's contents. The mirror of [`Slot::load`]; for a
    /// byref slot this REBINDS the pointer rather than writing through it, which C# admits only at
    /// a `ref` local's declaration (ref REASSIGNMENT is C# 7.3 and unbuilt).
    #[must_use]
    pub fn store(self) -> Instruction {
        match self {
            Slot::Argument(index) => Instruction::new(Opcode::Starg, Operand::Variable(index)),
            Slot::Local(index) => Instruction::new(Opcode::Stloc, Operand::Variable(index)),
        }
    }
}

/// A method's variable slots, keyed by name, with the local types in slot order.
#[derive(Debug, Default)]
pub struct Frame {
    slots: BTreeMap<Box<str>, Slot>,
    /// The local-variable types in slot order. Behind a `RefCell` so a compiler
    /// temporary (e.g. spilling a value-type rvalue receiver) can be reserved during
    /// expression emission, which holds the frame only by shared reference.
    local_types: RefCell<Vec<TypeSymbol>>,
    /// The source name of each local slot, parallel to `local_types` (empty for a compiler
    /// temporary). The debug local-variable table reads this. Keeping it per-slot rather than
    /// inverting the by-name `slots` map lets shadowed same-named locals -- two `catch (E e)`
    /// clauses, or a name reused across sibling blocks -- each carry their own name in their
    /// own slot instead of only the last-declared one being recorded.
    local_names: RefCell<Vec<Box<str>>>,
    /// The local slot reserved for each named declaration, keyed by its declaring span and name
    /// (a `Local` declarator, a `catch` variable, a `foreach`/`fixed` variable). Same-named locals
    /// in disjoint scopes each get a distinct slot here even though the by-name `slots` map keeps
    /// only the last-declared one; emission rebinds the name to its own declaration's slot via
    /// [`Frame::rebind_decl`] before that declaration's stores and its scope's reads, so a use
    /// resolves to the right slot instead of a same-named sibling's.
    decl_slots: BTreeMap<(u32, u32, Box<str>), u16>,
    /// The referent type of each byref (`ref`/`out`) parameter, by name. Such a
    /// parameter's argument slot holds an address: a read derefs it (`ldind`), a write
    /// stores through it (`stind`).
    byref_types: BTreeMap<Box<str>, TypeSymbol>,
    /// Local slots that must be PINNED in the signature -- a `fixed` statement's array
    /// holder, so the GC does not move the array while a pointer into it is live.
    pinned: RefCell<BTreeSet<u16>>,
    /// The local slot holding each spilled operand of the enclosing
    /// [`BoundExprKind::Sequence`](lamella_binder::BoundExprKind::Sequence)s, innermost scope
    /// last. A `Temp(i)` reads the innermost scope's `i`th slot.
    ///
    /// **A STACK RATHER THAN A MAP, BECAUSE SEQUENCES NEST AND THE INDEX IS POSITIONAL.**
    /// `(a + b) + c` puts one sequence inside another's operand list, and both number their
    /// operands from zero; a flat map would have the inner one overwrite the outer's slots and
    /// the outer's `Temp(0)` would then read the inner's spilled value. Behind a `RefCell` for
    /// the same reason `local_types` is: emission holds the frame by shared reference.
    temp_scopes: RefCell<Vec<Vec<u16>>>,
    /// The DECLARING TYPE's own type-parameter names, in declaration order, so emission can turn
    /// a `T` into the `!n` a token spells it with. Empty for a method of a non-generic type.
    type_parameters: Vec<Box<str>>,
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
        type_parameters: &[Box<str>],
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
        frame.type_parameters = type_parameters.to_vec();
        frame.collect_locals(body);
        frame
    }

    /// The POSITION of `name` in the declaring type's parameter list -- the `n` of the `!n` a
    /// metadata token spells it with (II.23.1.16) -- or `None` when the name is not one.
    ///
    /// **THIS IS THE ONE PLACE EMISSION TURNS A NAME INTO A NUMBER.** The binder works by name and
    /// metadata numbers, and everywhere else that meeting happens in `open_type_sig` at signature
    /// time. `default(T)` needs it at INSTRUCTION time too, because `initobj` takes a token and a
    /// bare `T` has none -- deliberately, since minting one would invent a type called `T`.
    #[must_use]
    pub fn type_parameter_index(&self, name: &str) -> Option<u32> {
        self.type_parameters
            .iter()
            .position(|parameter| &**parameter == name)
            .map(|index| index as u32)
    }

    /// The slot a name occupies, if any.
    #[must_use]
    pub fn slot(&self, name: &str) -> Option<Slot> {
        self.slots.get(name).copied()
    }

    /// Points `name` at the local slot reserved for the declaration at `span` (a no-op if none is
    /// recorded there). Emission calls this at each declaration so a name reused across disjoint
    /// scopes -- two `catch (E e)`, a local redeclared in a sibling block, two `foreach` variables
    /// -- resolves to THAT declaration's slot, not a same-named sibling's last-declared one. No
    /// restore is needed: a use always follows its declaration and precedes any sibling
    /// redeclaration (C# scoping forbids shadowing an enclosing local and makes sibling scopes
    /// disjoint), so the binding is correct wherever the name is legally read.
    pub fn rebind_decl(&mut self, span: Span, name: &str) {
        if let Some(&slot) = self.decl_slots.get(&(span.start, span.end, Box::from(name))) {
            self.slots.insert(name.into(), Slot::Local(slot));
        }
    }

    /// The slot and referent type of `name` when it holds a MANAGED POINTER rather than a value --
    /// a `ref`/`out` parameter or a `ref` LOCAL -- so a read derefs (`ldind`) and a write stores
    /// through it (`stind`).
    ///
    /// **THE SLOT IS RETURNED RATHER THAN ITS INDEX, SO A CALLER CANNOT CHOOSE THE LOAD
    /// INSTRUCTION WITHOUT DECIDING WHICH KIND OF SLOT IT HAS.** An index alone is the same `u16`
    /// for an argument and a local, and eight call sites read this: with a bare index, one that
    /// loads an argument slot with a local's number reads the wrong variable and compiles.
    /// [`Slot::load`] and [`Slot::store`] make the choice unspellable.
    ///
    /// **A LOCAL'S BYREF-NESS IS READ FROM ITS SLOT'S TYPE, NOT FROM A BY-NAME MAP**, so two
    /// same-named locals in sibling scopes -- one `ref int r`, one plain `int r` -- cannot be
    /// confused for each other. `byref_types` stays by name for PARAMETERS, where a name is unique
    /// in the method and there is no slot type to read.
    #[must_use]
    pub fn byref(&self, name: &str) -> Option<(Slot, TypeSymbol)> {
        match *self.slots.get(name)? {
            Slot::Argument(index) => {
                let ty = self.byref_types.get(name)?;
                Some((Slot::Argument(index), ty.clone()))
            }
            Slot::Local(index) => match self.local_types.borrow().get(index as usize) {
                Some(TypeSymbol::ByRef(referent)) => {
                    Some((Slot::Local(index), (**referent).clone()))
                }
                _ => None,
            },
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
    /// [`Frame::local_types`]; a compiler temporary keeps the empty default.)
    #[must_use]
    pub fn local_names(&self) -> Vec<Box<str>> {
        self.local_names.borrow().clone()
    }

    /// Records the source name of a local slot for the debug local-variable table. Each slot is
    /// named at most once (at its declaration), so shadowed same-named locals each keep their own
    /// name. A no-op for an out-of-range slot; a temporary that is never named keeps its empty
    /// default.
    pub fn name_local(&self, slot: u16, name: &str) {
        if let Some(entry) = self.local_names.borrow_mut().get_mut(slot as usize) {
            *entry = name.into();
        }
    }

    /// Reserves and names the slot for one declared local. A `ref` local arrives here with a
    /// `TypeSymbol::ByRef` type and needs no special case: the slot's type IS the fact, which is
    /// what makes its signature `T&` and what [`Frame::byref`] reads back.
    fn declare_local(&mut self, span: Span, name: &str, ty: &TypeSymbol) {
        let slot = self.reserve_local(ty);
        self.name_local(slot, name);
        self.slots.insert(name.into(), Slot::Local(slot));
        self.decl_slots
            .insert((span.start, span.end, name.into()), slot);
    }

    /// Reserves an unnamed local of `ty` (a compiler temporary, such as the value a
    /// `return` inside a `try` parks before leaving, or a spilled value-type rvalue
    /// receiver), returning its slot index. Takes `&self` so emission can reserve a
    /// temporary while holding the frame by shared reference.
    pub fn reserve_local(&self, ty: &TypeSymbol) -> u16 {
        let slot = {
            let mut locals = self.local_types.borrow_mut();
            let slot = locals.len() as u16;
            locals.push(ty.clone());
            slot
        };
        self.local_names.borrow_mut().push(Box::from(""));
        slot
    }

    /// Opens a scope for a [`BoundExprKind::Sequence`](lamella_binder::BoundExprKind::Sequence)'s
    /// spilled operands. Paired with [`Frame::close_temp_scope`].
    pub fn open_temp_scope(&self) {
        self.temp_scopes.borrow_mut().push(Vec::new());
    }

    /// Records `slot` as the next spilled operand of the innermost open scope, so a later
    /// `Temp(i)` finds it. Operands are bound one at a time as each is stored, so an operand may
    /// name the ones spilled before it.
    pub fn bind_temp(&self, slot: u16) {
        if let Some(scope) = self.temp_scopes.borrow_mut().last_mut() {
            scope.push(slot);
        }
    }

    /// Closes the innermost spilled-operand scope.
    pub fn close_temp_scope(&self) {
        self.temp_scopes.borrow_mut().pop();
    }

    /// The local slot of the innermost scope's `index`th spilled operand, or `None` when there is
    /// no such operand -- which means a `Temp` was emitted outside the `Sequence` that binds it,
    /// and the caller reports it rather than emitting a wrong slot.
    #[must_use]
    pub fn temp_slot(&self, index: u32) -> Option<u16> {
        self.temp_scopes
            .borrow()
            .last()
            .and_then(|scope| scope.get(index as usize).copied())
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
                    self.declare_local(stmt.span, &declarator.name, ty);
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
                self.declare_local(stmt.span, name, element_type);
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
                self.declare_local(stmt.span, name, &TypeSymbol::Pointer(Box::new(element.clone())));
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
                        self.declare_local(catch.span, name, &ty);
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
