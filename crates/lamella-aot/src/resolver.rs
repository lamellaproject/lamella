//! A [`CallResolver`] backed by a compiled assembly's metadata.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use lamella_cil::Operand;
use lamella_ir::{Function, MirType, TypeHandle};
use lamella_metadata::{Assembly, Method, MethodKind, ResolvedMethod, SigType, TargetLayout};

use crate::cil::{CallInfo, CallResolver, CallTarget, CilError, Intrinsic, lower_method_typed};

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

    fn value_type_size(&self, operand: &Operand) -> Option<u32> {
        let Operand::Token(token) = operand else {
            return None;
        };
        self.assembly
            .value_type_layout(*token, &TargetLayout::ilp32())
            .ok()
            .map(|layout| layout.size)
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
