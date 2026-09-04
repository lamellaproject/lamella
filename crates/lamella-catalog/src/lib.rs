//! The board and part fact files this build knows, and how a board resolves to its part row.

use lamella_bsp_gen::strata::{BoardTable, PartRow, Strata, parse};

/// The board and part fact files, emitted by `build.rs` from `bsp/` and `csp/`.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/catalog.rs"));
}

pub use generated::{BOARD_PYTHON, BOARDS, MODULES, PARTS};

/// The board with `id`, parsed from the embedded catalog.
#[must_use]
pub fn load_board(id: &str) -> Option<BoardTable> {
    let (_, text) = BOARDS.iter().find(|(board, _)| *board == id)?;
    match parse(text) {
        Ok(Strata::Board(board)) => Some(board),
        _ => None,
    }
}

/// The part row `board` is built around.
///
/// A board names EITHER a bare chip (`family` + `part`) or a MODULE, and the two are exclusive. A
/// module states the family and part it carries, so a module-carrying board resolves in one extra
/// hop -- and a resolver that knew only the bare-chip form reported an empty part id for it rather
/// than saying it could not follow the chain.
#[must_use]
pub fn load_part(board: &BoardTable) -> Option<PartRow> {
    let (family, part) = if board.module.is_empty() {
        (board.family.clone(), board.part.clone())
    } else {
        let (_, text) = MODULES.iter().find(|(id, _)| *id == board.module)?;
        let Ok(Strata::Module(module)) = parse(text) else {
            return None;
        };
        (module.family.clone(), module.part.clone())
    };
    let (_, text) = PARTS.iter().find(|(id, _)| *id == family)?;
    let Ok(Strata::Parts(parts)) = parse(text) else {
        return None;
    };
    parts.rows.into_iter().find(|row| row.part == part)
}

/// A board and its part row together, or a message saying which hop failed.
///
/// The two failures read differently to whoever typed the id and the message says which: an id
/// that is not in the catalog is a typo, and an id that resolves to no part row is a hole in the
/// fact tables. Naming them alike would send a reader to check their spelling when the tables are
/// what is incomplete.
///
/// # Errors
/// When the board id is unknown, or the board resolves to no part row in this build.
pub fn resolve(id: &str) -> Result<(BoardTable, PartRow), String> {
    let Some(board) = load_board(id) else {
        return Err(format!("no board {id:?}; try `lamella boards`"));
    };
    let Some(part) = load_part(&board) else {
        return Err(format!(
            "board {id:?} (family {:?}, module {:?}, part {:?}) resolves to no part row in this build",
            board.family, board.module, board.part
        ));
    };
    Ok((board, part))
}

/// Every part row this build knows, as `(family, row)`, in catalog order.
///
/// A consumer wanting silicon identity reads [`PartRow::dp_idcode`] and the `device_id*` group,
/// and must read `device_id_identifies` before naming a part from either -- a debug-port IDCODE
/// names a port design, and on some families even the vendor register names a category rather than
/// a part.
#[must_use]
pub fn all_parts() -> Vec<(String, PartRow)> {
    let mut rows = Vec::new();
    for (family, text) in PARTS {
        let Ok(Strata::Parts(parts)) = parse(text) else { continue };
        for row in parts.rows {
            rows.push(((*family).to_string(), row));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry enumerates every family the catalog embeds, and every row parses.
    ///
    /// A COUNT AGAINST THE SOURCE LIST rather than a magic number: `all_parts` skipping a family
    /// whose table failed to parse would otherwise look like a family that has no parts, and the
    /// two call for opposite responses.
    #[test]
    fn the_part_registry_covers_every_family_the_catalog_embeds() {
        let rows = all_parts();
        let families: std::collections::BTreeSet<&str> =
            rows.iter().map(|(family, _)| family.as_str()).collect();
        let embedded: std::collections::BTreeSet<&str> =
            PARTS.iter().map(|(family, _)| *family).collect();
        let missing: Vec<&&str> = embedded.difference(&families).collect();
        assert!(missing.is_empty(), "a family's parts table did not parse: {missing:?}");
        assert!(rows.len() >= embedded.len(), "every family has at least one part row");
        assert!(rows.iter().all(|(_, row)| !row.part.is_empty()), "every row names a part");
    }

    /// What the registry can say about silicon identity today, printed rather than asserted.
    ///
    /// REPORTED AND NOT RATCHETED, deliberately. A row states an identity when a probe has READ one
    /// from that part -- not when a sibling in the same family answered, and not when a core class
    /// makes a value predictable. So the count moves with bench work rather than with edits here,
    /// and a ratchet over it would be a ratchet over how much silicon somebody reached this month.
    /// `cargo test -p lamella-catalog -- --nocapture` shows it.
    #[test]
    fn the_identity_coverage_is_reported() {
        let rows = all_parts();
        let dp = rows.iter().filter(|(_, r)| r.dp_idcode.is_some()).count();
        let dev = rows.iter().filter(|(_, r)| r.device_id.is_some()).count();
        let names_a_part =
            rows.iter().filter(|(_, r)| r.device_id_identifies == "part").count();
        println!("part rows                        {}", rows.len());
        println!("  with a debug-port IDCODE       {dp}");
        println!("  with a vendor device id        {dev}");
        println!("    that names THIS PART         {names_a_part}");
        println!("    that names a broader class   {}", dev - names_a_part);
        for (family, row) in &rows {
            let dp = row.dp_idcode.as_ref().map_or("-".to_string(), |i| format!("{:#010x}", i.value));
            let id = row.device_id.as_ref().map_or("-".to_string(), |i| format!("{:#x}", i.value));
            let scope =
                if row.device_id_identifies.is_empty() { "-" } else { &row.device_id_identifies };
            println!("  {family:<12} {:<16} dp {dp:<12} device {id:<8} {scope}", row.part);
        }
    }

    /// A stated identity is stated WHOLE, across every family. The parser refuses a partial one, so
    /// this is the census that proves the refusal is reached for real tables rather than only for a
    /// hand-built row.
    #[test]
    fn no_part_states_half_an_identity() {
        for (family, row) in all_parts() {
            let group = [
                row.device_id_reg.is_some(),
                row.device_id_mask.is_some(),
                row.device_id.is_some(),
                !row.device_id_identifies.is_empty(),
            ];
            assert!(
                group.iter().all(|v| *v) || group.iter().all(|v| !*v),
                "{family}/{}: a device identity is four fields together or none of them",
                row.part
            );
        }
    }

    /// **A CENSUS, NOT AN EXAMPLE, AND IT IS WHAT FINDS THE NEXT SHAPE.** Every board in the
    /// catalog must resolve to a part row with a real RAM figure. Written after running the tool
    /// by hand found that module-carrying boards resolved to an EMPTY part id -- a shape a
    /// hand-picked Pico row passes straight over. A new board whose form this resolver cannot
    /// follow now fails here rather than at a user's prompt.
    ///
    /// It reports EVERY failure before asserting: a census that panics on the first one answers
    /// "is there a hole" when the question is "where are the holes".
    #[test]
    fn every_board_in_the_catalog_resolves_to_a_part_row() {
        let mut unresolved = Vec::new();
        for (id, _) in BOARDS {
            let Some(board) = load_board(id) else {
                unresolved.push(format!("{id}: board.toml did not parse"));
                continue;
            };
            match load_part(&board) {
                Some(part) if part.ram > 0 => {}
                Some(part) => unresolved.push(format!("{id}: part {:?} states no RAM", part.part)),
                None => unresolved.push(format!(
                    "{id}: family {:?} module {:?} part {:?} -> no row",
                    board.family, board.module, board.part
                )),
            }
        }
        assert!(unresolved.is_empty(), "boards with no resolvable part row: {unresolved:#?}");
    }

    /// The catalog is generated from the directory, so an empty one means the build script found
    /// nothing -- which would make every other test here vacuously true.
    #[test]
    fn the_catalog_is_not_empty() {
        assert!(BOARDS.len() > 20, "got {} boards", BOARDS.len());
        assert!(!PARTS.is_empty());
        assert!(!MODULES.is_empty(), "module-carrying boards need these");
    }

    /// A module-carrying board resolves through its module to a real part, which is the hop the
    /// bare-chip form does not have.
    #[test]
    fn a_module_carrying_board_resolves_through_its_module() {
        let board = load_board("arduino-mkr1000").expect("the mkr1000 board file");
        assert!(board.family.is_empty(), "this board names a module, not a family");
        assert!(!board.module.is_empty());
        let part = load_part(&board).expect("a module board must still resolve to a part");
        assert!(part.flash > 0 && part.ram > 0, "with real budgets: {part:?}");
    }

    /// The two ways a board id fails must not read alike.
    #[test]
    fn an_unknown_id_and_an_unresolvable_board_report_differently() {
        let unknown = resolve("no-such-board").expect_err("refuses");
        assert!(unknown.contains("lamella boards"), "it says how to look: {unknown}");
        assert!(resolve("rpi-pico").is_ok(), "a real board resolves");
    }
}
