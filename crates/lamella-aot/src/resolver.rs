//! A [`CallResolver`] backed by a compiled assembly's metadata.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use lamella_cil::Operand;
use lamella_metadata::{Assembly, MethodKind, ResolvedMethod, SigType};

use crate::cil::{CallInfo, CallResolver, CallTarget, Intrinsic};

/// Resolves an assembly's `call` and `ldstr` tokens against its metadata.
pub struct MetadataResolver<'a> {
    assembly: &'a Assembly<'a>,
}

impl<'a> MetadataResolver<'a> {
    /// Wraps an assembly to resolve its tokens.
    #[must_use]
    pub fn new(assembly: &'a Assembly<'a>) -> MetadataResolver<'a> {
        MetadataResolver { assembly }
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
        let target = match method.kind {
            MethodKind::Definition(rid) => CallTarget::Internal(rid),
            MethodKind::Reference if is_debug_writeline(&method) => {
                CallTarget::Intrinsic(Intrinsic::DebugWriteLine)
            }
            MethodKind::Reference => return None,
        };
        Some(CallInfo {
            args,
            has_result,
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
