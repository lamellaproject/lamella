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
