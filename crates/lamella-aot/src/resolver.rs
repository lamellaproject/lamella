//! A [`CallResolver`] backed by a compiled assembly's metadata.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use lamella_cil::Operand;
use lamella_ir::{Function, MirType, TypeHandle};
use lamella_metadata::tables::table;
use lamella_metadata::{
    Assembly, Method, MethodKind, ResolvedMethod, SigType, TargetLayout, TypeDef,
};
use lamella_token::Token;

use crate::cil::{
    Array2DOp, ArrayElement, CallInfo, CallResolver, CallTarget, CilError, Intrinsic,
    ReferenceLayout, lower_method_typed,
};

/// Resolves an assembly's `call` and `ldstr` tokens against its metadata.
pub struct MetadataResolver<'a> {
    assembly: &'a Assembly<'a>,
    /// For module lowering: each callee's `MethodDef` rid paired with its function index in
    /// the module. Empty for single-method lowering, where a call keeps its rid (a one-
    /// function lowering does not dispatch internal calls anyway).
    rid_to_index: Vec<(u32, u32)>,
}

impl<'a> MetadataResolver<'a> {
    /// Wraps an assembly to resolve the tokens of a single method (no inter-method calls).
    #[must_use]
    pub fn new(assembly: &'a Assembly<'a>) -> MetadataResolver<'a> {
        MetadataResolver {
            assembly,
            rid_to_index: Vec::new(),
        }
    }

    /// Wraps an assembly to resolve calls among the methods of a module: `method_rids` are
    /// their `MethodDef` rids in lowering order, so a call between them resolves to the
    /// callee's function index (what [`crate::cil::CallTarget::Internal`] names).
    #[must_use]
    pub fn for_module(assembly: &'a Assembly<'a>, method_rids: &[u32]) -> MetadataResolver<'a> {
        let rid_to_index = method_rids
            .iter()
            .enumerate()
            .map(|(index, &rid)| (rid, index as u32))
            .collect();
        MetadataResolver {
            assembly,
            rid_to_index,
        }
    }

    /// Maps a callee's `MethodDef` rid to its function index in the module, or passes the rid
    /// through for single-method lowering. `None` if the call names a method outside the
    /// module being lowered.
    fn function_index(&self, rid: u32) -> Option<u32> {
        if self.rid_to_index.is_empty() {
            Some(rid)
        } else {
            self.rid_to_index
                .iter()
                .find(|&&(r, _)| r == rid)
                .map(|&(_, index)| index)
        }
    }

    /// The `TypeDef` a `newobj` constructs, from its constructor token: the constructor's
    /// declaring type, found by name. Shared by the value-type and reference-type resolutions.
    fn newobj_type_def(&self, operand: &Operand) -> Option<TypeDef<'a>> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let declaring = self.assembly.resolve_method(*token)?.declaring_type?;
        self.assembly.find_type(declaring.namespace, declaring.name)
    }

    /// The type token a metadata token names: a type token as-is (`TypeRef`/`TypeDef`/
    /// `TypeSpec`), or the declaring type of a constructor token -- a `MemberRef`'s parent (an
    /// external type like `System.Exception`), or a `MethodDef`'s owning type resolved by name
    /// (a this-module exception). `None` for any other token.
    fn type_token_of(&self, token: Token) -> Option<Token> {
        match token.table() {
            table::TYPE_REF | table::TYPE_DEF | table::TYPE_SPEC => Some(token),
            table::MEMBER_REF => Some(self.assembly.member_ref(token.row())?.parent()),
            table::METHOD_DEF => {
                let name = self.assembly.resolve_method(token)?.declaring_type?;
                Some(self.assembly.find_type(name.namespace, name.name)?.token())
            }
            _ => None,
        }
    }

    /// Whether `type_token` names an exception type, for the no-GC tag model's `newobj`/`catch`
    /// recognition: a `System.*Exception` (the BCL exceptions live in another assembly the tag
    /// model never needs to walk into, so they are matched by name), or a this-module type whose
    /// `extends` chain reaches one. The walk is bounded so a malformed cyclic base cannot loop.
    fn is_exception_type(&self, type_token: Token) -> bool {
        let mut current = type_token;
        for _ in 0..64 {
            let Some(name) = self.assembly.type_token_name(current) else {
                return false;
            };
            if name.namespace == "System"
                && (name.name == "Exception" || name.name.ends_with("Exception"))
            {
                return true;
            }
            if current.table() != table::TYPE_DEF {
                return false;
            }
            let Some(type_def) = self.assembly.type_def(current.row()) else {
                return false;
            };
            let base = type_def.extends();
            if base.row() == 0 {
                return false;
            }
            current = base;
        }
        false
    }
}

impl CallResolver for MetadataResolver<'_> {
    fn resolve(&self, operand: &Operand) -> Option<CallInfo> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let method = self.assembly.resolve_method(*token)?;
        let signature = method.signature.as_ref()?;
        let args = signature.parameters.len() + usize::from(signature.has_this);
        let has_result = !matches!(signature.return_type, SigType::Void);
        let result_type = has_result
            .then(|| {
                mir_type(
                    &signature.return_type,
                    self.assembly,
                    &TargetLayout::ilp32(),
                )
            })
            .flatten();
        let target = match method.kind {
            MethodKind::Definition(rid) => CallTarget::Internal(self.function_index(rid)?),
            MethodKind::Reference if is_debug_writeline(&method) => {
                CallTarget::Intrinsic(Intrinsic::DebugWriteLine)
            }
            MethodKind::Reference if is_console_writeline_int(&method) => {
                CallTarget::Intrinsic(Intrinsic::ConsoleWriteLineInt)
            }
            MethodKind::Reference if is_string_op_equality(&method) => {
                CallTarget::Intrinsic(Intrinsic::StringEquals)
            }
            MethodKind::Reference if is_string_concat(&method) => {
                CallTarget::Intrinsic(Intrinsic::StringConcat)
            }
            MethodKind::Reference if is_int32_tostring(&method) => {
                CallTarget::Intrinsic(Intrinsic::IntToString)
            }
            MethodKind::Reference if is_object_ctor(&method) => {
                CallTarget::Intrinsic(Intrinsic::ObjectCtor)
            }
            MethodKind::Reference if is_array_getlength(&method) => {
                CallTarget::Intrinsic(Intrinsic::ArrayGetLength)
            }
            MethodKind::Reference => return None,
        };
        Some(CallInfo {
            args,
            has_result,
            result_type,
            target,
        })
    }

    fn user_string(&self, operand: &Operand) -> Option<Box<[u8]>> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let raw = self.assembly.image().user_strings().get(token.row()).ok()?;
        Some(decode_user_string(raw).into_bytes().into_boxed_slice())
    }

    fn field_offset(&self, operand: &Operand) -> Option<u32> {
        let Operand::Token(token) = operand else {
            return None;
        };
        self.assembly.field_offset(*token, &TargetLayout::ilp32())
    }

    fn field_type(&self, operand: &Operand) -> Option<MirType> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let signature = self.assembly.field_signature(*token)?;
        mir_type(&signature, self.assembly, &TargetLayout::ilp32())
    }

    fn value_type_size(&self, operand: &Operand) -> Option<u32> {
        let Operand::Token(token) = operand else {
            return None;
        };
        self.assembly
            .value_type_layout(*token, &TargetLayout::ilp32())
            .ok()
            .map(|layout| layout.size)
    }

    fn newobj_value_type(&self, operand: &Operand) -> Option<MirType> {
        let type_def = self.newobj_type_def(operand)?;
        if !type_def.is_value_type() {
            return None;
        }
        let layout = self
            .assembly
            .value_type_layout(type_def.token(), &TargetLayout::ilp32())
            .ok()?;
        Some(MirType::ValueType {
            handle: TypeHandle(type_def.token().0),
            size: layout.size,
        })
    }

    fn newobj_reference_layout(&self, operand: &Operand) -> Option<ReferenceLayout> {
        let type_def = self.newobj_type_def(operand)?;
        if type_def.is_value_type() {
            return None;
        }
        let layout = self
            .assembly
            .value_type_layout(type_def.token(), &TargetLayout::ilp32())
            .ok()?;
        Some(ReferenceLayout {
            handle: TypeHandle(type_def.token().0),
            size: layout.size,
            reference_offsets: layout.reference_offsets,
        })
    }

    fn array_element(&self, operand: &Operand) -> Option<ArrayElement> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let by_layout = || {
            self.assembly
                .value_type_layout(*token, &TargetLayout::ilp32())
                .map(|layout| layout.size)
                .unwrap_or(4)
        };
        let element_size = match self.assembly.type_token_name(*token) {
            Some(name) if name.namespace == "System" => match name.name {
                "Boolean" | "SByte" | "Byte" => 1,
                "Int16" | "UInt16" | "Char" => 2,
                "Int32" | "UInt32" | "Single" => 4,
                "Int64" | "UInt64" | "Double" => 8,
                _ => by_layout(),
            },
            _ => by_layout(),
        };
        Some(ArrayElement {
            handle: TypeHandle(token.0),
            element_size,
        })
    }

    fn array_2d_op(&self, operand: &Operand) -> Option<Array2DOp> {
        let Operand::Token(token) = operand else {
            return None;
        };
        if token.table() != table::MEMBER_REF {
            return None;
        }
        let member = self.assembly.member_ref(token.row())?;
        let parent = member.parent();
        let SigType::Array { element, rank } = self.assembly.type_spec_signature(parent)? else {
            return None;
        };
        if rank != 2 {
            return None;
        }
        let (element_size, signed) = array_element_size(&element);
        match member.name()? {
            ".ctor" => Some(Array2DOp::New {
                handle: TypeHandle(parent.0),
                element_size,
            }),
            "Get" => Some(Array2DOp::Get {
                element_size,
                signed,
                element_type: mir_type(&element, self.assembly, &TargetLayout::ilp32())
                    .unwrap_or(MirType::I32),
            }),
            "Set" => Some(Array2DOp::Set { element_size }),
            _ => None,
        }
    }

    fn static_field_offset(&self, operand: &Operand) -> Option<u32> {
        let Operand::Token(token) = operand else {
            return None;
        };
        (token.table() == table::FIELD).then(|| token.row() * 4)
    }

    fn exception_tag(&self, operand: &Operand) -> Option<u32> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let type_token = self.type_token_of(*token)?;
        if !self.is_exception_type(type_token) {
            return None;
        }
        let tag = self.assembly.exception_tag(type_token);
        (tag != 0).then_some(tag)
    }

    fn is_catch_all_type(&self, operand: &Operand) -> bool {
        let Operand::Token(token) = operand else {
            return false;
        };
        self.type_token_of(*token)
            .and_then(|type_token| self.assembly.type_token_name(type_token))
            .is_some_and(|name| {
                name.namespace == "System" && matches!(name.name, "Exception" | "Object")
            })
    }

    fn boxed_layout(&self, operand: &Operand) -> Option<ReferenceLayout> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let handle = TypeHandle(token.0);
        if let Some(name) = self.assembly.type_token_name(*token) {
            if name.namespace == "System" {
                let size = match name.name {
                    "Boolean" | "SByte" | "Byte" => Some(1),
                    "Int16" | "UInt16" | "Char" => Some(2),
                    "Int32" | "UInt32" | "Single" => Some(4),
                    "Int64" | "UInt64" | "Double" => Some(8),
                    _ => None,
                };
                if let Some(size) = size {
                    return Some(ReferenceLayout {
                        handle,
                        size,
                        reference_offsets: Vec::new(),
                    });
                }
            }
        }
        let layout = self
            .assembly
            .value_type_layout(*token, &TargetLayout::ilp32())
            .ok()?;
        Some(ReferenceLayout {
            handle,
            size: layout.size,
            reference_offsets: layout.reference_offsets,
        })
    }
}

/// The byte width and signedness of a primitive 2-D array element (a sub-word `Get` sign- or
/// zero-extends per the flag); references and unhandled element types fall back to a 4-byte slot.
fn array_element_size(element: &SigType) -> (u32, bool) {
    match element {
        SigType::I1 => (1, true),
        SigType::Boolean | SigType::U1 => (1, false),
        SigType::I2 => (2, true),
        SigType::Char | SigType::U2 => (2, false),
        SigType::I4 => (4, true),
        SigType::U4 | SigType::R4 => (4, false),
        SigType::I8 | SigType::U8 | SigType::R8 => (8, false),
        _ => (4, false),
    }
}

/// Maps a metadata [`SigType`] to the MIR type the AOT lowers it as. `None` for `void` and
/// for types the backend does not lower yet (a value type in another assembly, arrays).
fn mir_type(sig: &SigType, assembly: &Assembly, target: &TargetLayout) -> Option<MirType> {
    Some(match sig {
        SigType::Boolean
        | SigType::Char
        | SigType::I1
        | SigType::U1
        | SigType::I2
        | SigType::U2
        | SigType::I4
        | SigType::U4 => MirType::I32,
        SigType::I8 | SigType::U8 => MirType::I64,
        SigType::R4 => MirType::F32,
        SigType::R8 => MirType::F64,
        SigType::IntPtr | SigType::UIntPtr => MirType::NativeInt,
        SigType::Class(_) | SigType::Object | SigType::String => MirType::ObjectRef,
        SigType::SzArray(_) | SigType::Array { .. } => MirType::ObjectRef,
        SigType::ValueType(token) => MirType::ValueType {
            handle: TypeHandle(token.0),
            size: assembly.value_type_layout(*token, target).ok()?.size,
        },
        _ => return None,
    })
}

/// Lowers the given methods of an `assembly` to MIR as one module: a call from one of them
/// to another resolves to the callee's index in `methods` (so pass them in the order you
/// will give a module lowering such as [`crate::arm32::lower_module`], the entry first), and
/// each method's arguments and locals are typed from its signature.
///
/// Errors if a method has no CIL body, or if a body cannot be lowered.
pub fn lower_methods(assembly: &Assembly, methods: &[Method]) -> Result<Vec<Function>, CilError> {
    let rids: Vec<u32> = methods.iter().map(Method::rid).collect();
    let resolver = MetadataResolver::for_module(assembly, &rids);
    let target = TargetLayout::ilp32();
    methods
        .iter()
        .map(|method| {
            let body = method.body().ok_or(CilError::MissingBody)?;
            let (arg_types, local_types) = slot_types(assembly, method, &target);
            lower_method_typed(&body, &resolver, &arg_types, &local_types).map(|(func, _)| func)
        })
        .collect()
}

/// A method's argument and local MIR types, from its signature and local-variable
/// signature; a type the backend does not lower yet falls back to `int32`.
fn slot_types(
    assembly: &Assembly,
    method: &Method,
    target: &TargetLayout,
) -> (Vec<MirType>, Vec<MirType>) {
    let mut arg_types = Vec::new();
    if let Some(signature) = method.signature() {
        if signature.has_this {
            arg_types.push(MirType::ManagedPtr);
        }
        for param in &signature.parameters {
            arg_types.push(mir_type(param, assembly, target).unwrap_or(MirType::I32));
        }
    }
    let local_types = method
        .local_variables()
        .iter()
        .map(|local| mir_type(local, assembly, target).unwrap_or(MirType::I32))
        .collect();
    (arg_types, local_types)
}

/// Whether a resolved method is `System.Diagnostics.Debug.WriteLine`.
fn is_debug_writeline(method: &ResolvedMethod) -> bool {
    method.name == Some("WriteLine")
        && method
            .declaring_type
            .is_some_and(|t| t.namespace == "System.Diagnostics" && t.name == "Debug")
}

/// Whether a resolved method is `System.Console.WriteLine(int)` -- the single-`int` overload,
/// distinguished from the many other `WriteLine` overloads by its parameter type.
fn is_console_writeline_int(method: &ResolvedMethod) -> bool {
    method.name == Some("WriteLine")
        && method
            .declaring_type
            .is_some_and(|t| t.namespace == "System" && t.name == "Console")
        && method
            .signature
            .as_ref()
            .is_some_and(|sig| matches!(sig.parameters.as_slice(), [SigType::I4]))
}

/// Whether a resolved method is `System.Object::.ctor()` -- the base constructor a derived
/// constructor chains to, which the lowering treats as a no-op.
fn is_object_ctor(method: &ResolvedMethod) -> bool {
    method.name == Some(".ctor")
        && method
            .declaring_type
            .is_some_and(|t| t.namespace == "System" && t.name == "Object")
}

/// Whether a resolved method is `System.Array::GetLength(int)` -- the per-dimension length accessor
/// (used to loop over an array, including `int[,]`); the lowering reads it from the array header.
fn is_array_getlength(method: &ResolvedMethod) -> bool {
    method.name == Some("GetLength")
        && method
            .declaring_type
            .is_some_and(|t| t.namespace == "System" && t.name == "Array")
}

/// Whether a resolved method is `System.String::op_Equality(string, string)` (the `==` operator).
fn is_string_op_equality(method: &ResolvedMethod) -> bool {
    method.name == Some("op_Equality")
        && method
            .declaring_type
            .is_some_and(|t| t.namespace == "System" && t.name == "String")
        && method.signature.as_ref().is_some_and(|sig| {
            matches!(
                sig.parameters.as_slice(),
                [SigType::String, SigType::String]
            )
        })
}

/// Whether a resolved method is a fixed-arity `System.String::Concat(string, ...)` -- the 2-, 3-, or
/// 4-string overloads `a + b`, `a + b + c`, `a + b + c + d` emit. The front end chains it pairwise.
/// (The `Concat(string[])` params-array and `Concat(object...)` overloads are not yet recognized.)
fn is_string_concat(method: &ResolvedMethod) -> bool {
    method.name == Some("Concat")
        && method
            .declaring_type
            .is_some_and(|t| t.namespace == "System" && t.name == "String")
        && method.signature.as_ref().is_some_and(|sig| {
            (2..=4).contains(&sig.parameters.len())
                && sig.parameters.iter().all(|p| matches!(p, SigType::String))
        })
}

/// Whether a resolved method is `System.Int32::ToString()` -- the no-argument decimal formatter
/// (`i.ToString()`). The receiver is a managed pointer to the int. (The format-string and
/// `IFormatProvider` overloads are not recognized.)
fn is_int32_tostring(method: &ResolvedMethod) -> bool {
    method.name == Some("ToString")
        && method
            .declaring_type
            .is_some_and(|t| t.namespace == "System" && t.name == "Int32")
        && method
            .signature
            .as_ref()
            .is_some_and(|sig| sig.parameters.is_empty())
}

/// Decodes a `#US` entry (UTF-16 code units plus a trailing flag byte) to a [`String`].
fn decode_user_string(raw: &[u8]) -> String {
    let units = raw.len().saturating_sub(1) / 2;
    let utf16: Vec<u16> = (0..units)
        .map(|i| u16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]))
        .collect();
    String::from_utf16_lossy(&utf16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_user_string() {
        assert_eq!(decode_user_string(&[0x48, 0x00, 0x69, 0x00, 0x00]), "Hi");
    }

}
