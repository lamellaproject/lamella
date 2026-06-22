//! The CIL opcode table (ECMA-335 1st edition, Partition III, Table 10).

/// The `0xFE` byte that introduces every two-byte CIL opcode (ECMA-335 1st ed,
/// III.1.2.1).
pub const EXTENDED_PREFIX: u8 = 0xFE;

/// The kind of inline operand that follows an opcode in the instruction stream
/// (ECMA-335 1st ed, III.1.2).
///
/// This classifies the *physical* encoding -- how many bytes follow and how to
/// read them -- not the semantic role. The opcode itself implies whether a
/// [`OperandKind::Token`] names a method, field, type, string, or signature, so
/// the codec need not distinguish those: they are all a 4-byte token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperandKind {
    /// No inline operand.
    None,
    /// A signed 1-byte integer (`ldc.i4.s`).
    Int8,
    /// A signed 4-byte integer (`ldc.i4`).
    Int32,
    /// A signed 8-byte integer (`ldc.i8`).
    Int64,
    /// A 4-byte IEEE-754 float (`ldc.r4`).
    Float32,
    /// An 8-byte IEEE-754 float (`ldc.r8`).
    Float64,
    /// A 1-byte local-variable or argument slot number (the `.s` short forms).
    ShortVariable,
    /// A 2-byte local-variable or argument slot number (the `0xFE`-prefixed
    /// forms).
    Variable,
    /// A signed 1-byte branch displacement (the `.s` short branches and
    /// `leave.s`).
    ShortTarget,
    /// A signed 4-byte branch displacement (the long branches and `leave`).
    Target,
    /// A `switch` jump table: a 4-byte count `n` followed by `n` 4-byte
    /// displacements.
    Switch,
    /// A 4-byte metadata token (`call`, `ldfld`, `ldstr`, `ldtoken`, ...).
    Token,
    /// A 1-byte alignment for the `unaligned.` prefix (1, 2, or 4).
    Alignment,
}

impl OperandKind {
    /// The number of operand bytes that follow the opcode, when that count is
    /// fixed.
    ///
    /// Returns `Some(0)` for [`OperandKind::None`] and `None` for
    /// [`OperandKind::Switch`], whose length depends on its jump-table size
    /// (`4 + 4 * n`).
    #[must_use]
    pub fn fixed_operand_len(self) -> Option<usize> {
        Some(match self {
            OperandKind::None => 0,
            OperandKind::Int8
            | OperandKind::ShortVariable
            | OperandKind::ShortTarget
            | OperandKind::Alignment => 1,
            OperandKind::Variable => 2,
            OperandKind::Int32
            | OperandKind::Float32
            | OperandKind::Target
            | OperandKind::Token => 4,
            OperandKind::Int64 | OperandKind::Float64 => 8,
            OperandKind::Switch => return None,
        })
    }
}

/// How an opcode is encoded in the instruction stream (ECMA-335 1st ed,
/// III.1.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    /// A one-byte opcode with the given value (in `0x00..=0xE0`).
    Single(u8),
    /// A two-byte opcode: the [`EXTENDED_PREFIX`] followed by the given byte.
    Extended(u8),
}

impl Encoding {
    /// The number of bytes the opcode itself occupies (1 or 2), before any
    /// operand.
    #[must_use]
    pub fn byte_len(self) -> usize {
        match self {
            Encoding::Single(_) => 1,
            Encoding::Extended(_) => 2,
        }
    }
}

/// Defines the [`Opcode`] enum from a table of rows, each `Variant = key,
/// "mnemonic", OperandKind`. The `key` is the packed encoding: the single opcode
/// byte for a one-byte opcode, or `0xFE00 | b` for the two-byte form. Driving the
/// enum, its mnemonics, its operand kinds, and the decode lookup from one table
/// keeps them from drifting apart.
macro_rules! opcodes {
    (
        $(#[$enum_meta:meta])*
        $vis:vis enum $name:ident {
            $( $variant:ident = $key:literal, $mnemonic:literal, $operand:ident ; )+
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name {
            $(
                #[doc = concat!("The `", $mnemonic, "` instruction.")]
                $variant,
            )+
        }

        impl $name {
            /// This opcode's canonical assembly mnemonic (ECMA-335 1st ed,
            /// Partition III).
            #[must_use]
            pub fn mnemonic(self) -> &'static str {
                match self {
                    $( $name::$variant => $mnemonic, )+
                }
            }

            /// The kind of inline operand that follows this opcode in the stream.
            #[must_use]
            pub fn operand_kind(self) -> OperandKind {
                match self {
                    $( $name::$variant => OperandKind::$operand, )+
                }
            }

            /// Every opcode, in ascending encoding order.
            #[must_use]
            pub fn all() -> &'static [$name] {
                &[ $( $name::$variant, )+ ]
            }

            /// The packed encoding key: the single opcode byte for a one-byte
            /// opcode, or `0xFE00 | b` for the two-byte (`0xFE`-prefixed) form.
            #[must_use]
            pub(crate) const fn key(self) -> u16 {
                match self {
                    $( $name::$variant => $key, )+
                }
            }

            /// The opcode for packed encoding `key`, if one is defined.
            #[must_use]
            pub(crate) fn from_key(key: u16) -> Option<$name> {
                match key {
                    $( $key => Some($name::$variant), )+
                    _ => None,
                }
            }
        }
    };
}

opcodes! {
    /// A CIL instruction opcode (ECMA-335 1st edition, Partition III, Table 10).
    ///
    /// The set is complete for the 1st-edition standard and grows as the project
    /// ladders to later CLI editions; it is deliberately not `#[non_exhaustive]`
    /// so that a consumer's `match` is rechecked when an edition adds opcodes.
    pub enum Opcode {
        Nop = 0x00, "nop", None;
        Break = 0x01, "break", None;
        Ldarg0 = 0x02, "ldarg.0", None;
        Ldarg1 = 0x03, "ldarg.1", None;
        Ldarg2 = 0x04, "ldarg.2", None;
        Ldarg3 = 0x05, "ldarg.3", None;
        Ldloc0 = 0x06, "ldloc.0", None;
        Ldloc1 = 0x07, "ldloc.1", None;
        Ldloc2 = 0x08, "ldloc.2", None;
        Ldloc3 = 0x09, "ldloc.3", None;
        Stloc0 = 0x0A, "stloc.0", None;
        Stloc1 = 0x0B, "stloc.1", None;
        Stloc2 = 0x0C, "stloc.2", None;
        Stloc3 = 0x0D, "stloc.3", None;
        LdargS = 0x0E, "ldarg.s", ShortVariable;
        LdargaS = 0x0F, "ldarga.s", ShortVariable;
        StargS = 0x10, "starg.s", ShortVariable;
        LdlocS = 0x11, "ldloc.s", ShortVariable;
        LdlocaS = 0x12, "ldloca.s", ShortVariable;
        StlocS = 0x13, "stloc.s", ShortVariable;
        Ldnull = 0x14, "ldnull", None;
        LdcI4M1 = 0x15, "ldc.i4.m1", None;
        LdcI40 = 0x16, "ldc.i4.0", None;
        LdcI41 = 0x17, "ldc.i4.1", None;
        LdcI42 = 0x18, "ldc.i4.2", None;
        LdcI43 = 0x19, "ldc.i4.3", None;
        LdcI44 = 0x1A, "ldc.i4.4", None;
        LdcI45 = 0x1B, "ldc.i4.5", None;
        LdcI46 = 0x1C, "ldc.i4.6", None;
        LdcI47 = 0x1D, "ldc.i4.7", None;
        LdcI48 = 0x1E, "ldc.i4.8", None;
        LdcI4S = 0x1F, "ldc.i4.s", Int8;
        LdcI4 = 0x20, "ldc.i4", Int32;
        LdcI8 = 0x21, "ldc.i8", Int64;
        LdcR4 = 0x22, "ldc.r4", Float32;
        LdcR8 = 0x23, "ldc.r8", Float64;
        Dup = 0x25, "dup", None;
        Pop = 0x26, "pop", None;
        Jmp = 0x27, "jmp", Token;
        Call = 0x28, "call", Token;
        Calli = 0x29, "calli", Token;
        Ret = 0x2A, "ret", None;
        BrS = 0x2B, "br.s", ShortTarget;
        BrfalseS = 0x2C, "brfalse.s", ShortTarget;
        BrtrueS = 0x2D, "brtrue.s", ShortTarget;
        BeqS = 0x2E, "beq.s", ShortTarget;
        BgeS = 0x2F, "bge.s", ShortTarget;
        BgtS = 0x30, "bgt.s", ShortTarget;
        BleS = 0x31, "ble.s", ShortTarget;
        BltS = 0x32, "blt.s", ShortTarget;
        BneUnS = 0x33, "bne.un.s", ShortTarget;
        BgeUnS = 0x34, "bge.un.s", ShortTarget;
        BgtUnS = 0x35, "bgt.un.s", ShortTarget;
        BleUnS = 0x36, "ble.un.s", ShortTarget;
        BltUnS = 0x37, "blt.un.s", ShortTarget;
        Br = 0x38, "br", Target;
        Brfalse = 0x39, "brfalse", Target;
        Brtrue = 0x3A, "brtrue", Target;
        Beq = 0x3B, "beq", Target;
        Bge = 0x3C, "bge", Target;
        Bgt = 0x3D, "bgt", Target;
        Ble = 0x3E, "ble", Target;
        Blt = 0x3F, "blt", Target;
        BneUn = 0x40, "bne.un", Target;
        BgeUn = 0x41, "bge.un", Target;
        BgtUn = 0x42, "bgt.un", Target;
        BleUn = 0x43, "ble.un", Target;
        BltUn = 0x44, "blt.un", Target;
        Switch = 0x45, "switch", Switch;
        LdindI1 = 0x46, "ldind.i1", None;
        LdindU1 = 0x47, "ldind.u1", None;
        LdindI2 = 0x48, "ldind.i2", None;
        LdindU2 = 0x49, "ldind.u2", None;
        LdindI4 = 0x4A, "ldind.i4", None;
        LdindU4 = 0x4B, "ldind.u4", None;
        LdindI8 = 0x4C, "ldind.i8", None;
        LdindI = 0x4D, "ldind.i", None;
        LdindR4 = 0x4E, "ldind.r4", None;
        LdindR8 = 0x4F, "ldind.r8", None;
        LdindRef = 0x50, "ldind.ref", None;
        StindRef = 0x51, "stind.ref", None;
        StindI1 = 0x52, "stind.i1", None;
        StindI2 = 0x53, "stind.i2", None;
        StindI4 = 0x54, "stind.i4", None;
        StindI8 = 0x55, "stind.i8", None;
        StindR4 = 0x56, "stind.r4", None;
        StindR8 = 0x57, "stind.r8", None;
        Add = 0x58, "add", None;
        Sub = 0x59, "sub", None;
        Mul = 0x5A, "mul", None;
        Div = 0x5B, "div", None;
        DivUn = 0x5C, "div.un", None;
        Rem = 0x5D, "rem", None;
        RemUn = 0x5E, "rem.un", None;
        And = 0x5F, "and", None;
        Or = 0x60, "or", None;
        Xor = 0x61, "xor", None;
        Shl = 0x62, "shl", None;
        Shr = 0x63, "shr", None;
        ShrUn = 0x64, "shr.un", None;
        Neg = 0x65, "neg", None;
        Not = 0x66, "not", None;
        ConvI1 = 0x67, "conv.i1", None;
        ConvI2 = 0x68, "conv.i2", None;
        ConvI4 = 0x69, "conv.i4", None;
        ConvI8 = 0x6A, "conv.i8", None;
        ConvR4 = 0x6B, "conv.r4", None;
        ConvR8 = 0x6C, "conv.r8", None;
        ConvU4 = 0x6D, "conv.u4", None;
        ConvU8 = 0x6E, "conv.u8", None;
        Callvirt = 0x6F, "callvirt", Token;
        Cpobj = 0x70, "cpobj", Token;
        Ldobj = 0x71, "ldobj", Token;
        Ldstr = 0x72, "ldstr", Token;
        Newobj = 0x73, "newobj", Token;
        Castclass = 0x74, "castclass", Token;
        Isinst = 0x75, "isinst", Token;
        ConvRUn = 0x76, "conv.r.un", None;
        Unbox = 0x79, "unbox", Token;
        Throw = 0x7A, "throw", None;
        Ldfld = 0x7B, "ldfld", Token;
        Ldflda = 0x7C, "ldflda", Token;
        Stfld = 0x7D, "stfld", Token;
        Ldsfld = 0x7E, "ldsfld", Token;
        Ldsflda = 0x7F, "ldsflda", Token;
        Stsfld = 0x80, "stsfld", Token;
        Stobj = 0x81, "stobj", Token;
        ConvOvfI1Un = 0x82, "conv.ovf.i1.un", None;
        ConvOvfI2Un = 0x83, "conv.ovf.i2.un", None;
        ConvOvfI4Un = 0x84, "conv.ovf.i4.un", None;
        ConvOvfI8Un = 0x85, "conv.ovf.i8.un", None;
        ConvOvfU1Un = 0x86, "conv.ovf.u1.un", None;
        ConvOvfU2Un = 0x87, "conv.ovf.u2.un", None;
        ConvOvfU4Un = 0x88, "conv.ovf.u4.un", None;
        ConvOvfU8Un = 0x89, "conv.ovf.u8.un", None;
        ConvOvfIUn = 0x8A, "conv.ovf.i.un", None;
        ConvOvfUUn = 0x8B, "conv.ovf.u.un", None;
        Box = 0x8C, "box", Token;
        Newarr = 0x8D, "newarr", Token;
        Ldlen = 0x8E, "ldlen", None;
        Ldelema = 0x8F, "ldelema", Token;
        LdelemI1 = 0x90, "ldelem.i1", None;
        LdelemU1 = 0x91, "ldelem.u1", None;
        LdelemI2 = 0x92, "ldelem.i2", None;
        LdelemU2 = 0x93, "ldelem.u2", None;
        LdelemI4 = 0x94, "ldelem.i4", None;
        LdelemU4 = 0x95, "ldelem.u4", None;
        LdelemI8 = 0x96, "ldelem.i8", None;
        LdelemI = 0x97, "ldelem.i", None;
        LdelemR4 = 0x98, "ldelem.r4", None;
        LdelemR8 = 0x99, "ldelem.r8", None;
        LdelemRef = 0x9A, "ldelem.ref", None;
        StelemI = 0x9B, "stelem.i", None;
        StelemI1 = 0x9C, "stelem.i1", None;
        StelemI2 = 0x9D, "stelem.i2", None;
        StelemI4 = 0x9E, "stelem.i4", None;
        StelemI8 = 0x9F, "stelem.i8", None;
        StelemR4 = 0xA0, "stelem.r4", None;
        StelemR8 = 0xA1, "stelem.r8", None;
        StelemRef = 0xA2, "stelem.ref", None;
        Ldelem = 0xA3, "ldelem", Token;
        Stelem = 0xA4, "stelem", Token;
        UnboxAny = 0xA5, "unbox.any", Token;
        ConvOvfI1 = 0xB3, "conv.ovf.i1", None;
        ConvOvfU1 = 0xB4, "conv.ovf.u1", None;
        ConvOvfI2 = 0xB5, "conv.ovf.i2", None;
        ConvOvfU2 = 0xB6, "conv.ovf.u2", None;
        ConvOvfI4 = 0xB7, "conv.ovf.i4", None;
        ConvOvfU4 = 0xB8, "conv.ovf.u4", None;
        ConvOvfI8 = 0xB9, "conv.ovf.i8", None;
        ConvOvfU8 = 0xBA, "conv.ovf.u8", None;
        Refanyval = 0xC2, "refanyval", Token;
        Ckfinite = 0xC3, "ckfinite", None;
        Mkrefany = 0xC6, "mkrefany", Token;
        Ldtoken = 0xD0, "ldtoken", Token;
        ConvU2 = 0xD1, "conv.u2", None;
        ConvU1 = 0xD2, "conv.u1", None;
        ConvI = 0xD3, "conv.i", None;
        ConvOvfI = 0xD4, "conv.ovf.i", None;
        ConvOvfU = 0xD5, "conv.ovf.u", None;
        AddOvf = 0xD6, "add.ovf", None;
        AddOvfUn = 0xD7, "add.ovf.un", None;
        MulOvf = 0xD8, "mul.ovf", None;
        MulOvfUn = 0xD9, "mul.ovf.un", None;
        SubOvf = 0xDA, "sub.ovf", None;
        SubOvfUn = 0xDB, "sub.ovf.un", None;
        Endfinally = 0xDC, "endfinally", None;
        Leave = 0xDD, "leave", Target;
        LeaveS = 0xDE, "leave.s", ShortTarget;
        StindI = 0xDF, "stind.i", None;
        ConvU = 0xE0, "conv.u", None;

        Arglist = 0xFE00, "arglist", None;
        Ceq = 0xFE01, "ceq", None;
        Cgt = 0xFE02, "cgt", None;
        CgtUn = 0xFE03, "cgt.un", None;
        Clt = 0xFE04, "clt", None;
        CltUn = 0xFE05, "clt.un", None;
        Ldftn = 0xFE06, "ldftn", Token;
        Ldvirtftn = 0xFE07, "ldvirtftn", Token;
        Ldarg = 0xFE09, "ldarg", Variable;
        Ldarga = 0xFE0A, "ldarga", Variable;
        Starg = 0xFE0B, "starg", Variable;
        Ldloc = 0xFE0C, "ldloc", Variable;
        Ldloca = 0xFE0D, "ldloca", Variable;
        Stloc = 0xFE0E, "stloc", Variable;
        Localloc = 0xFE0F, "localloc", None;
        Endfilter = 0xFE11, "endfilter", None;
        Unaligned = 0xFE12, "unaligned.", Alignment;
        Volatile = 0xFE13, "volatile.", None;
        Tail = 0xFE14, "tail.", None;
        Initobj = 0xFE15, "initobj", Token;
        Constrained = 0xFE16, "constrained.", Token;
        Cpblk = 0xFE17, "cpblk", None;
        Initblk = 0xFE18, "initblk", None;
        Rethrow = 0xFE1A, "rethrow", None;
        Sizeof = 0xFE1C, "sizeof", Token;
        Refanytype = 0xFE1D, "refanytype", None;
        Readonly = 0xFE1E, "readonly.", None;
    }
}

impl Opcode {
    /// How this opcode is encoded in the instruction stream.
    #[must_use]
    pub fn encoding(self) -> Encoding {
        let key = self.key();
        if key >> 8 == EXTENDED_PREFIX as u16 {
            Encoding::Extended(key as u8)
        } else {
            Encoding::Single(key as u8)
        }
    }

    /// The opcode named by a single byte that is not the [`EXTENDED_PREFIX`], if
    /// one is defined.
    #[must_use]
    pub fn from_single(byte: u8) -> Option<Opcode> {
        if byte == EXTENDED_PREFIX {
            return None;
        }
        Opcode::from_key(byte as u16)
    }

    /// The opcode named by the byte that follows an [`EXTENDED_PREFIX`], if one
    /// is defined.
    #[must_use]
    pub fn from_extended(byte: u8) -> Option<Opcode> {
        Opcode::from_key(((EXTENDED_PREFIX as u16) << 8) | byte as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeSet;

    #[test]
    fn there_are_218_opcodes() {
        assert_eq!(Opcode::all().len(), 218);
    }

    #[test]
    fn encodings_are_unique() {
        let mut seen = BTreeSet::new();
        for &opcode in Opcode::all() {
            assert!(
                seen.insert(opcode.key()),
                "duplicate encoding for {}",
                opcode.mnemonic()
            );
        }
        assert_eq!(seen.len(), Opcode::all().len());
    }

    #[test]
    fn mnemonics_are_unique() {
        let mut seen = BTreeSet::new();
        for &opcode in Opcode::all() {
            assert!(
                seen.insert(opcode.mnemonic()),
                "duplicate {}",
                opcode.mnemonic()
            );
        }
    }

    #[test]
    fn every_opcode_decodes_from_its_own_encoding() {
        for &opcode in Opcode::all() {
            let decoded = match opcode.encoding() {
                Encoding::Single(byte) => Opcode::from_single(byte),
                Encoding::Extended(byte) => Opcode::from_extended(byte),
            };
            assert_eq!(decoded, Some(opcode), "{}", opcode.mnemonic());
        }
    }

    #[test]
    fn one_byte_and_two_byte_opcodes_split_at_the_prefix() {
        let two_byte = Opcode::all()
            .iter()
            .filter(|op| matches!(op.encoding(), Encoding::Extended(_)))
            .count();
        assert_eq!(two_byte, 27);
        assert_eq!(Opcode::all().len() - two_byte, 191);
    }

    #[test]
    fn known_encodings_match_the_standard() {
        assert_eq!(Opcode::Nop.encoding(), Encoding::Single(0x00));
        assert_eq!(Opcode::Add.encoding(), Encoding::Single(0x58));
        assert_eq!(Opcode::Ret.encoding(), Encoding::Single(0x2A));
        assert_eq!(Opcode::ConvU.encoding(), Encoding::Single(0xE0));
        assert_eq!(Opcode::Arglist.encoding(), Encoding::Extended(0x00));
        assert_eq!(Opcode::Ceq.encoding(), Encoding::Extended(0x01));
        assert_eq!(Opcode::Ldarg.encoding(), Encoding::Extended(0x09));
        assert_eq!(Opcode::Refanytype.encoding(), Encoding::Extended(0x1D));
    }

    #[test]
    fn decode_rejects_undefined_and_misused_bytes() {
        assert_eq!(Opcode::from_single(0x24), None);
        assert_eq!(Opcode::from_single(0x77), None);
        assert_eq!(Opcode::from_single(EXTENDED_PREFIX), None);
        assert_eq!(Opcode::from_extended(0x08), None);
        assert_eq!(Opcode::from_extended(0x10), None);
    }

    #[test]
    fn operand_kinds_match_the_standard() {
        assert_eq!(Opcode::Nop.operand_kind(), OperandKind::None);
        assert_eq!(Opcode::LdcI4S.operand_kind(), OperandKind::Int8);
        assert_eq!(Opcode::LdcI4.operand_kind(), OperandKind::Int32);
        assert_eq!(Opcode::LdcI8.operand_kind(), OperandKind::Int64);
        assert_eq!(Opcode::LdcR4.operand_kind(), OperandKind::Float32);
        assert_eq!(Opcode::LdcR8.operand_kind(), OperandKind::Float64);
        assert_eq!(Opcode::LdargS.operand_kind(), OperandKind::ShortVariable);
        assert_eq!(Opcode::Ldarg.operand_kind(), OperandKind::Variable);
        assert_eq!(Opcode::BrS.operand_kind(), OperandKind::ShortTarget);
        assert_eq!(Opcode::Br.operand_kind(), OperandKind::Target);
        assert_eq!(Opcode::Switch.operand_kind(), OperandKind::Switch);
        assert_eq!(Opcode::Call.operand_kind(), OperandKind::Token);
        assert_eq!(Opcode::Ldstr.operand_kind(), OperandKind::Token);
        assert_eq!(Opcode::Ldtoken.operand_kind(), OperandKind::Token);
        assert_eq!(Opcode::Unaligned.operand_kind(), OperandKind::Alignment);
    }

    #[test]
    fn fixed_operand_lengths_match_the_encoding() {
        assert_eq!(OperandKind::None.fixed_operand_len(), Some(0));
        assert_eq!(OperandKind::Int8.fixed_operand_len(), Some(1));
        assert_eq!(OperandKind::ShortVariable.fixed_operand_len(), Some(1));
        assert_eq!(OperandKind::ShortTarget.fixed_operand_len(), Some(1));
        assert_eq!(OperandKind::Alignment.fixed_operand_len(), Some(1));
        assert_eq!(OperandKind::Variable.fixed_operand_len(), Some(2));
        assert_eq!(OperandKind::Int32.fixed_operand_len(), Some(4));
        assert_eq!(OperandKind::Float32.fixed_operand_len(), Some(4));
        assert_eq!(OperandKind::Target.fixed_operand_len(), Some(4));
        assert_eq!(OperandKind::Token.fixed_operand_len(), Some(4));
        assert_eq!(OperandKind::Int64.fixed_operand_len(), Some(8));
        assert_eq!(OperandKind::Float64.fixed_operand_len(), Some(8));
        assert_eq!(OperandKind::Switch.fixed_operand_len(), None);
    }

    #[test]
    fn encoding_byte_length_follows_the_prefix() {
        assert_eq!(Opcode::Add.encoding().byte_len(), 1);
        assert_eq!(Opcode::Ceq.encoding().byte_len(), 2);
    }
}
