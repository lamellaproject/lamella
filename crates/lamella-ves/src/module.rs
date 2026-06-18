//! A minimal, interim module: the methods the interpreter runs and calls between,

use crate::interp::Vm;
use crate::trap::Trap;
use crate::value::Value;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use lamella_cil::MethodBodyImage;
use lamella_token::Token;

/// An index identifying a [`Method`] within a [`Module`].
pub type MethodId = u32;

/// A native (runtime-implemented) method: the intrinsic ABI.
///
/// This is the seam a BCL method crosses to reach Rust -- `System.Console.Write`
/// and friends are intrinsics of this shape. The function receives the runtime
/// context [`Vm`] (heap, console, ...) and the call arguments in declaration
/// order, and returns the method's result (`None` for `void`) or a [`Trap`]. It is
/// documented as a shared seam in `docs/COORDINATION.md`.
pub type IntrinsicFn = fn(&mut Vm, &[Value]) -> Result<Option<Value>, Trap>;

/// A callable method: either managed CIL the interpreter executes, or a native
/// intrinsic it invokes directly.
#[derive(Clone)]
pub enum Method {
    /// Managed CIL: the decoded body and its argument count.
    Managed {
        /// The method's decoded CIL body.
        body: MethodBodyImage,
        /// How many arguments the method takes (a resolved signature replaces this
        /// once `lamella-metadata` is wired in).
        arg_count: u16,
    },
    /// A native intrinsic implemented in Rust.
    Intrinsic {
        /// The Rust implementation.
        func: IntrinsicFn,
        /// How many arguments it takes from the caller's stack.
        arg_count: u16,
    },
}

impl Method {
    /// How many arguments this method takes from the caller's evaluation stack.
    #[must_use]
    pub fn arg_count(&self) -> u16 {
        match self {
            Method::Managed { arg_count, .. } | Method::Intrinsic { arg_count, .. } => *arg_count,
        }
    }
}

/// A collection of methods, the call tokens that name them, and the strings
/// `ldstr` loads.
#[derive(Clone, Default)]
pub struct Module {
    methods: Vec<Method>,
    by_token: BTreeMap<u32, MethodId>,
    strings: BTreeMap<u32, Box<[u16]>>,
}

impl Module {
    /// Creates an empty module.
    #[must_use]
    pub fn new() -> Module {
        Module::default()
    }

    /// Adds a managed method and returns its [`MethodId`].
    pub fn add_method(&mut self, body: MethodBodyImage, arg_count: u16) -> MethodId {
        self.push(Method::Managed { body, arg_count })
    }

    /// Adds a native intrinsic and returns its [`MethodId`].
    pub fn add_intrinsic(&mut self, func: IntrinsicFn, arg_count: u16) -> MethodId {
        self.push(Method::Intrinsic { func, arg_count })
    }

    fn push(&mut self, method: Method) -> MethodId {
        let id = self.methods.len() as MethodId;
        self.methods.push(method);
        id
    }

    /// Binds a `call` token to the method it resolves to (standing in for
    /// metadata's `MethodDef`/`MemberRef` resolution).
    pub fn bind_token(&mut self, token: Token, method: MethodId) {
        self.by_token.insert(token.0, method);
    }

    /// Binds an `ldstr` token to the UTF-16 string it loads (standing in for the
    /// `#US` user-string heap).
    pub fn bind_string(&mut self, token: Token, chars: &[u16]) {
        self.strings.insert(token.0, chars.into());
    }

    /// The method a `call` token resolves to, if any.
    #[must_use]
    pub fn resolve(&self, token: Token) -> Option<MethodId> {
        self.by_token.get(&token.0).copied()
    }

    /// The string an `ldstr` token loads, if any.
    #[must_use]
    pub fn resolve_string(&self, token: Token) -> Option<&[u16]> {
        self.strings.get(&token.0).map(AsRef::as_ref)
    }

    /// The method with the given id, if it exists.
    #[must_use]
    pub fn method(&self, id: MethodId) -> Option<&Method> {
        self.methods.get(id as usize)
    }
}
