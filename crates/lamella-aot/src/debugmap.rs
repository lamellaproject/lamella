//! The AOT source-line debug map: native code offsets paired with the C# source lines they
//! lowered from -- what a source-level debugger needs (the in-browser on-device debugger and
//! the native VS Code one). This is Stage 1 of the debug contract: compose the lowering's
//! native-offset -> CIL-byte-offset line table with a Portable PDB's CIL-offset -> source-line
//! sequence points.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use lamella_metadata::PortablePdb;

/// One source-position row: a native code offset (relative to the method's code start -- the
/// consumer adds the device load base) and the 1-based C# source line and column it lowered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLine {
    /// The native code offset.
    pub addr: u32,
    /// The 1-based source line.
    pub line: u32,
    /// The 1-based source column of the statement (0 if the PDB gives none).
    pub col: u32,
}

/// Builds the source-position rows for the method `rid` from its `line_table` (native offset ->
/// CIL byte offset, from the arm32 debug lowering) and the Portable PDB. Sorted by `addr`, with
/// consecutive rows at the same (line, column) coalesced, so each row is the lowest address of a
/// source-statement run -- exactly where a breakpoint goes.
#[must_use]
pub fn source_lines(rid: u32, line_table: &[(u32, u32)], pdb: &PortablePdb) -> Vec<SourceLine> {
    let rows: Vec<SourceLine> = line_table
        .iter()
        .filter_map(|&(addr, cil)| {
            let point = pdb.source_location(rid, cil)?;
            Some(SourceLine {
                addr,
                line: point.start_line,
                col: point.start_column,
            })
        })
        .collect();
    coalesce(rows)
}

/// Sorts by address and drops a row whose (line, column) repeats the previous one, so each surviving
/// row is the LOWEST address of a source-statement run -- exactly where a breakpoint goes.
fn coalesce(mut rows: Vec<SourceLine>) -> Vec<SourceLine> {
    rows.sort_by_key(|row| row.addr);
    let mut coalesced: Vec<SourceLine> = Vec::with_capacity(rows.len());
    for row in rows {
        if coalesced.last().map(|prev| (prev.line, prev.col)) != Some((row.line, row.col)) {
            coalesced.push(row);
        }
    }
    coalesced
}

/// One method's source positions, resolved from a Portable PDB AHEAD of lowering: the file it came
/// from and its sequence points as `(CIL byte offset, line, column)`.
///
/// Resolving the PDB up front is what keeps `lamella-metadata` out of the code generators. The
/// mapping is independent of where code lands, so it can be computed before lowering starts and
/// handed down; the generator then composes it with the native-offset -> CIL-offset table only it
/// can produce.
#[derive(Debug, Clone, Copy, Default)]
pub struct MethodSource<'a> {
    /// The method's name as a debugger should SHOW it (`Program.Add`), which is not its symbol name
    /// (`f2` on the object path). The two are independent: a relocation names the symbol by index,
    /// while `DW_AT_name` is what a human reads. Empty falls back to the symbol name.
    pub name: &'a str,
    /// The source file the method lowered from.
    pub file: &'a str,
    /// `(CIL byte offset, 1-based line, 1-based column)`, in any order.
    pub points: &'a [(u32, u32, u32)],
    /// The method's named locals, in slot order. Empty for a method whose PDB says nothing.
    pub locals: &'a [LocalSlot],
    /// The method's named parameters, in ARGUMENT order -- see [`param_slots`]. A separate list
    /// from `locals` because the two are indexed by different things and DWARF spells them with
    /// different tags; merging them would need a discriminator at every use.
    pub params: &'a [LocalSlot],
}

/// A DWARF base type: the three facts a debugger needs before it can print a value the way the
/// source declared it, rather than as a machine word.
///
/// The size and encoding are not decoration. `bool`, `char` and `sbyte` occupy one to two bytes and
/// mean three different things, and a debugger told only "integer, 4 bytes" prints `65` where the
/// program says `'A'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BaseType {
    /// The name a debugger shows -- the C# keyword, because that is what the programmer wrote.
    pub name: &'static str,
    /// `DW_ATE_*` (DWARF 5 table 7.11).
    pub encoding: u8,
    /// The declared width in bytes.
    pub byte_size: u8,
}

/// One local a debugger can name, joined from the two halves that describe it: the Portable PDB
/// supplies the NAME and the slot it belongs to, the CIL local signature supplies that slot's TYPE.
///
/// **The join key is the slot index, and it is the whole risk in this structure.** An off-by-one
/// there produces a file every structural check accepts, in which each local wears its neighbour's
/// type -- so a name and a type have to be checked TOGETHER, against the source that declared them.
/// Counting either alone cannot tell a correct join from a shifted one.
#[derive(Debug, Clone)]
pub struct LocalSlot {
    /// The CIL local slot this describes -- the `ldloc`/`stloc` index, KEPT rather than implied by
    /// this entry's position in the list.
    ///
    /// **The position is not the slot, and assuming it is would be the off-by-one this type's
    /// warning is about.** A method whose slots 0, 1 and 3 are named (slot 2 being a compiler
    /// temporary the PDB does not name) produces three entries, and the third is slot 3. Anything
    /// joining a further per-slot fact -- where the local LIVES, above all -- has to join on this.
    pub slot: u16,
    /// The local's source name.
    pub name: String,
    /// Its type, or `None` where this backend cannot yet describe one -- a reference or a value
    /// type, which need a DIE this emitter does not build. Such a local is emitted with a name and
    /// no `DW_AT_type`, which DWARF permits and a debugger reads as "type unknown". **That is the
    /// honest answer and it is deliberately not a guess**: describing an object reference as a
    /// 32-bit integer would be a confidently wrong one, and a wrong answer ranks below an absent
    /// one for anyone reading a locals window.
    pub ty: Option<BaseType>,
}

/// The DWARF base type a CIL local-signature type is described by, or `None` for a type this
/// emitter cannot yet name.
///
/// **The types come from the SIGNATURE and never from the lowered MIR, and that is load-bearing.**
/// `resolver.rs`'s `mir_type_across` maps `Boolean`, `Char`, `I1`, `U1`, `I2` and `U2` all to
/// `MirType::I32`, because the CIL evaluation stack has no narrower integer -- so a type read off
/// the MIR reports six different source types as one 32-bit integer.
#[must_use]
pub fn base_type_of(signature: &lamella_metadata::SigType) -> Option<BaseType> {
    use lamella_metadata::SigType;
    const BOOLEAN: u8 = 0x02;
    const FLOAT: u8 = 0x04;
    const SIGNED: u8 = 0x05;
    const UNSIGNED: u8 = 0x07;
    const UTF: u8 = 0x10;
    let (name, encoding, byte_size) = match signature {
        SigType::Boolean => ("bool", BOOLEAN, 1),
        SigType::Char => ("char", UTF, 2),
        SigType::I1 => ("sbyte", SIGNED, 1),
        SigType::U1 => ("byte", UNSIGNED, 1),
        SigType::I2 => ("short", SIGNED, 2),
        SigType::U2 => ("ushort", UNSIGNED, 2),
        SigType::I4 => ("int", SIGNED, 4),
        SigType::U4 => ("uint", UNSIGNED, 4),
        SigType::I8 => ("long", SIGNED, 8),
        SigType::U8 => ("ulong", UNSIGNED, 8),
        SigType::R4 => ("float", FLOAT, 4),
        SigType::R8 => ("double", FLOAT, 8),
        _ => return None,
    };
    Some(BaseType {
        name,
        encoding,
        byte_size,
    })
}

/// Joins a method's PDB local NAMES to its signature local TYPES, both keyed by slot index.
///
/// A name whose slot the signature does not cover keeps `None` for its type rather than borrowing
/// a neighbour's: the two tables come from different files, and a PDB that disagrees with its
/// assembly is exactly the case where a confident answer would be wrong.
#[must_use]
pub fn local_slots(
    named: &[lamella_metadata::LocalVariable<'_>],
    signature: &[lamella_metadata::SigType],
) -> Vec<LocalSlot> {
    let mut slots: Vec<(u16, LocalSlot)> = named
        .iter()
        .map(|local| {
            (
                local.index,
                LocalSlot {
                    slot: local.index,
                    name: String::from(local.name),
                    ty: signature.get(local.index as usize).and_then(base_type_of),
                },
            )
        })
        .collect();
    slots.sort_by_key(|(index, _)| *index);
    slots.into_iter().map(|(_, slot)| slot).collect()
}

/// Joins a method's PARAMETER names to its signature's parameter types, keyed by the argument index
/// the LOWERING uses -- the same shape [`local_slots`] produces for locals, and consumed the same
/// way.
///
/// # TWO OFF-BY-ONES MEET HERE AND THEY POINT IN OPPOSITE DIRECTIONS
///
/// A `Param` row's `Sequence` is 1-based and **sequence 0 is the RETURN value's row** (ECMA-335
/// II.22.33), so the signature's parameter list is indexed by `sequence - 1`. The lowering's
/// argument list is a different list again: an instance method's argument 0 is `this`, which has no
/// `Param` row at all, so its argument index is `sequence - 1 + 1`. One subtraction to reach the
/// type, one addition to reach the home, and they do not cancel.
///
/// Getting either wrong produces a file every structural check accepts, in which each parameter
/// wears its neighbour's type or reads its neighbour's value -- which is why the acceptance for this
/// stops a debugger in an INSTANCE method and a static one and reads both.
///
/// `this` itself is not described. It has no name in the metadata and its type is a reference this
/// emitter cannot name, so there is nothing to say about it that would not be invented.
#[must_use]
pub fn param_slots(named: &[(u32, &str)], signature: &lamella_metadata::MethodSig) -> Vec<LocalSlot> {
    let this = usize::from(signature.has_this);
    let mut slots: Vec<(u16, LocalSlot)> = Vec::new();
    for &(sequence, name) in named {
        let Some(position) = (sequence as usize).checked_sub(1) else {
            continue;
        };
        let Ok(index) = u16::try_from(position + this) else {
            continue;
        };
        slots.push((
            index,
            LocalSlot {
                slot: index,
                name: String::from(name),
                ty: signature.parameters.get(position).and_then(base_type_of),
            },
        ));
    }
    slots.sort_by_key(|(index, _)| *index);
    slots.into_iter().map(|(_, slot)| slot).collect()
}

/// Everything a code generator needs to emit DWARF for the object it is building. `None` at the
/// call site means "no debug info", which is every build that did not ask for it.
#[derive(Debug, Clone, Copy)]
pub struct ObjectDebug<'a> {
    /// Method `i`'s [`CilSourceMap`](crate::cil::CilSourceMap) rows, for the LOWERING to thread.
    pub source_maps: &'a [crate::cil::CilSourceMap],
    /// Method `i`'s resolved source positions, for the DWARF to name.
    pub methods: &'a [MethodSource<'a>],
    /// The compilation unit's name -- conventionally its primary source file.
    pub unit_name: &'a str,
    /// The `DW_AT_producer` string.
    pub producer: &'a str,
}

/// The method's OPENING source position -- the `(line, column)` of its EARLIEST sequence point --
/// or `None` when it has none.
///
/// It is what covers a function's prologue, which no row can: the rows describe positions the
/// lowered code reaches, and a prologue is code no source construct produced. See
/// [`crate::dwarf::FunctionLines::entry`], which is where it is consumed.
///
/// `points` is [`MethodSource::points`], which is documented as being in ANY order -- hence the
/// minimum rather than the first.
#[must_use]
pub fn entry_position(points: &[(u32, u32, u32)]) -> Option<(u32, u32)> {
    points
        .iter()
        .min_by_key(|&&(il_offset, ..)| il_offset)
        .map(|&(_, line, column)| (line, column))
}

/// [`entry_position`] for a method read straight out of `pdb` -- one rule, for the caller that
/// holds the PDB rather than a resolved [`MethodSource`]. Hidden points are dropped here, which is
/// the same filter the callers that build a `MethodSource` apply.
#[must_use]
pub fn pdb_entry_position(rid: u32, pdb: &PortablePdb) -> Option<(u32, u32)> {
    let points: Vec<(u32, u32, u32)> = pdb
        .sequence_points(rid)
        .into_iter()
        .filter(|p| !p.is_hidden)
        .map(|p| (p.il_offset, p.start_line, p.start_column))
        .collect();
    entry_position(&points)
}

/// Composes a method's native-offset -> CIL-offset table with its resolved source positions, and
/// rebases each row to be relative to `func_start`.
///
/// The rebase is what a DWARF line-number sequence needs: the sequence's base address comes from a
/// relocation against the function's symbol, so its rows advance from the FUNCTION's start, not from
/// the start of the object that happens to contain it.
#[must_use]
pub fn function_rows(
    line_table: &[(u32, u32)],
    source: &MethodSource,
    func_start: u32,
) -> Vec<SourceLine> {
    let rows: Vec<SourceLine> = line_table
        .iter()
        .filter_map(|&(addr, cil)| {
            let &(_, line, col) = source
                .points
                .iter()
                .filter(|&&(at, ..)| at <= cil)
                .max_by_key(|&&(at, ..)| at)?;
            Some(SourceLine {
                addr: addr.saturating_sub(func_start),
                line,
                col,
            })
        })
        .collect();
    coalesce(rows)
}

/// One method's contribution to a whole-image source map: its name, its source file, and the byte
/// offset of its code within the image (so a consumer maps an image PC to `addr = pc - offset`).
#[derive(Debug, Clone)]
pub struct MethodMap<'a> {
    /// The method's display name (e.g. `Program.Main`).
    pub name: &'a str,
    /// The source file the method lowered from.
    pub file: &'a str,
    /// The method's code offset within the image.
    pub image_offset: u32,
    /// The method-relative source-position rows from [`source_lines`].
    pub rows: &'a [SourceLine],
}

/// Serializes one method's source-line map to the web debugger's Stage-1 JSON sidecar:
/// `{ "lines": [ { "addr": <u32>, "file": <string>, "line": <u32>, "col": <u32> }, ... ] }`.
/// Each `addr` is relative to the method's code start.
#[must_use]
pub fn source_map_json(file: &str, rows: &[SourceLine]) -> String {
    let file = json_escape(file);
    let mut out = String::from("{\"lines\":[");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"addr\":{},\"file\":\"{file}\",\"line\":{},\"col\":{}}}",
            row.addr, row.line, row.col
        ));
    }
    out.push_str("]}");
    out
}

/// Serializes a whole image's source map -- every method's rows keyed by name, file, and image
/// offset -- so a consumer with the loaded image maps any PC to its source position:
/// `{ "methods": [ { "name", "file", "offset", "lines": [ { "addr", "line", "col" } ] } ] }`.
/// `addr` stays method-relative; the consumer finds the method whose `offset` precedes the PC.
#[must_use]
pub fn module_source_map_json(methods: &[MethodMap]) -> String {
    let mut out = String::from("{\"methods\":[");
    for (i, m) in methods.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"file\":\"{}\",\"offset\":{},\"lines\":[",
            json_escape(m.name),
            json_escape(m.file),
            m.image_offset
        ));
        for (j, row) in m.rows.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"addr\":{},\"line\":{},\"col\":{}}}",
                row.addr, row.line, row.col
            ));
        }
        out.push_str("]}");
    }
    out.push_str("]}");
    out
}

/// Escapes a string for a JSON string literal (quotes, backslashes -- Windows source paths have
/// them -- and control characters).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parameter_joins_its_type_by_sequence_and_its_home_by_argument_index() {
        use lamella_metadata::{MethodSig, SigType};
        let statik = MethodSig {
            has_this: false,
            explicit_this: false,
            return_type: SigType::I4,
            parameters: alloc::vec![SigType::I4, SigType::R8],
            is_vararg: false,
            return_type_required_modifiers: Vec::new(),
            required_modifiers: Vec::new(),
            generic_param_count: 0,
            sentinel_index: None,
        };
        let slots = param_slots(&[(0, "ret"), (1, "whole"), (2, "twice")], &statik);
        assert_eq!(slots.len(), 2);
        assert_eq!((slots[0].slot, slots[0].name.as_str()), (0, "whole"));
        assert_eq!(slots[0].ty.map(|t| t.name), Some("int"));
        assert_eq!((slots[1].slot, slots[1].name.as_str()), (1, "twice"));
        assert_eq!(slots[1].ty.map(|t| t.name), Some("double"));

        let instance = MethodSig {
            has_this: true,
            ..statik
        };
        let slots = param_slots(&[(1, "whole"), (2, "twice")], &instance);
        assert_eq!((slots[0].slot, slots[0].name.as_str()), (1, "whole"));
        assert_eq!(slots[0].ty.map(|t| t.name), Some("int"));
        assert_eq!((slots[1].slot, slots[1].name.as_str()), (2, "twice"));
        assert_eq!(slots[1].ty.map(|t| t.name), Some("double"));
    }

    #[test]
    fn serializes_the_stage_one_json() {
        let rows = [
            SourceLine {
                addr: 0,
                line: 5,
                col: 9,
            },
            SourceLine {
                addr: 8,
                line: 6,
                col: 13,
            },
        ];
        assert_eq!(
            source_map_json("Program.cs", &rows),
            r#"{"lines":[{"addr":0,"file":"Program.cs","line":5,"col":9},{"addr":8,"file":"Program.cs","line":6,"col":13}]}"#
        );
    }

    #[test]
    fn escapes_a_windows_path() {
        let rows = [SourceLine {
            addr: 4,
            line: 1,
            col: 1,
        }];
        assert_eq!(
            source_map_json("E:\\src\\Program.cs", &rows),
            r#"{"lines":[{"addr":4,"file":"E:\\src\\Program.cs","line":1,"col":1}]}"#
        );
    }

    #[test]
    fn serializes_a_whole_image_map() {
        let main_rows = [SourceLine {
            addr: 0,
            line: 5,
            col: 9,
        }];
        let helper_rows = [SourceLine {
            addr: 0,
            line: 12,
            col: 5,
        }];
        let methods = [
            MethodMap {
                name: "Program.Main",
                file: "P.cs",
                image_offset: 0x40,
                rows: &main_rows,
            },
            MethodMap {
                name: "Program.Helper",
                file: "P.cs",
                image_offset: 0x80,
                rows: &helper_rows,
            },
        ];
        assert_eq!(
            module_source_map_json(&methods),
            r#"{"methods":[{"name":"Program.Main","file":"P.cs","offset":64,"lines":[{"addr":0,"line":5,"col":9}]},{"name":"Program.Helper","file":"P.cs","offset":128,"lines":[{"addr":0,"line":12,"col":5}]}]}"#
        );
    }
}
