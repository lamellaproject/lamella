//! The board and part fact files this build knows, and how a board resolves to its part row.

use lamella_bsp_gen::strata::{BoardTable, PartRow, Strata, parse};

/// The board and part fact files, emitted by `build.rs` from `bsp/` and `csp/`.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/catalog.rs"));
}

pub use generated::{BOARD_PYTHON, BOARDS, BUS_TABLES, MODULES, PARTS};

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
    let (family, part) = chip_of(board)?;
    let (_, text) = PARTS.iter().find(|(id, _)| *id == family)?;
    let Ok(Strata::Parts(parts)) = parse(text) else {
        return None;
    };
    parts.rows.into_iter().find(|row| row.part == part)
}

/// The chip family `board` is built around: its own `family`, or the family of the module it
/// carries. `None` when the board names a module this build does not have.
#[must_use]
pub fn family_of(board: &BoardTable) -> Option<String> {
    chip_of(board).map(|(family, _)| family)
}

/// The bus table an image built for board `id` carries, or `None` when the board's family has no
/// table: the words the host writes into the baked image, and links beside an AOT image.
#[must_use]
pub fn bus_table(id: &str) -> Option<&'static [u32]> {
    BUS_TABLES.iter().find(|(board, _)| *board == id).map(|(_, table)| *table)
}

/// The board's chip as `(family, part)`, following a module-carrying board through its module:
/// the one place that hop is made.
fn chip_of(board: &BoardTable) -> Option<(String, String)> {
    if board.module.is_empty() {
        return Some((board.family.clone(), board.part.clone()));
    }
    let (_, text) = MODULES.iter().find(|(id, _)| *id == board.module)?;
    let Ok(Strata::Module(module)) = parse(text) else {
        return None;
    };
    Some((module.family, module.part))
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
/// THE CHIP REGISTRY, AND IT IS DERIVED RATHER THAN STORED. `lamella://chips` is built from the
/// part definitions themselves, so it cannot become a second source of truth about silicon that
/// disagrees with the identity values the probe and DAP crates work from.
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

/// Where a program's console and its log go on a board when its project names nowhere else, in the
/// words a project and a build's record spell it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleChannel {
    /// The console ring in RAM, which a debug probe reads while the program runs, without halting
    /// it.
    Memory,
    /// The UART the board's own debugger carries to a USB serial port of its own.
    Probe,
}

impl ConsoleChannel {
    /// The channel's word: `memory` or `probe`.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            ConsoleChannel::Memory => "memory",
            ConsoleChannel::Probe => "probe",
        }
    }
}

/// The boards whose debugger carries a UART that no image built for them writes: their console
/// takes the ring, so a default never names a wire nothing writes.
const DEBUGGER_UART_UNWRITTEN: [&str; 2] = ["bbc-micro-bit-v1", "bbc-micro-bit-v2"];

/// The channel `board`'s console and log take when its project names none.
///
/// `probe` where the board's own debugger carries a UART dedicated to it to a USB serial port: the
/// board has an on-board debugger, a carrier of a debugger's serial-bridge kind (one whose kind
/// ends in `-vcp`), and the UART binding that carrier rides -- and an image for the board can write
/// that UART. `memory` everywhere else. The default never takes a UART the board gives a program,
/// since the program may be using it.
#[must_use]
pub fn console_default(board: &BoardTable) -> ConsoleChannel {
    if debugger_carries_a_uart(board) && !DEBUGGER_UART_UNWRITTEN.contains(&board.board.as_str()) {
        ConsoleChannel::Probe
    } else {
        ConsoleChannel::Memory
    }
}

/// Whether `board`'s own debugger carries a UART dedicated to it to a USB serial port, as the
/// board's facts state it: an on-board debugger, a `-vcp` carrier, and a UART binding for the
/// carrier's role.
fn debugger_carries_a_uart(board: &BoardTable) -> bool {
    let has_debugger = board.debug.as_ref().is_some_and(|debug| !debug.onboard.is_empty() && debug.onboard != "none");
    has_debugger
        && board.carriers.iter().any(|carrier| {
            carrier.kind.ends_with("-vcp")
                && !carrier.role.is_empty()
                && board.bindings.iter().any(|binding| binding.role == carrier.role && binding.kind == "uart")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `id`'s default channel, as its word.
    fn default_word(id: &str) -> &'static str {
        console_default(&load_board(id).unwrap_or_else(|| panic!("{id}: board.toml parses"))).word()
    }

    /// **EVERY BOARD LAMELLA RHU BUILDS FOR TAKES RHU's CHANNEL**: `memory` on the four RP2350
    /// boards, and `probe`, the UART wired to the on-board ST-LINK's serial port, on the NUCLEO-U5A5
    /// and the F769 Discovery.
    #[test]
    fn the_boards_rhu_builds_for_take_rhus_channel() {
        for (board, word) in [
            ("rpi-pico2", "memory"),
            ("rpi-pico2-w", "memory"),
            ("pimoroni-pico-plus-2", "memory"),
            ("pimoroni-pico-plus-2-w", "memory"),
            ("st-nucleo-u5a5zj-q", "probe"),
            ("st-stm32f769i-disco", "probe"),
        ] {
            assert_eq!(default_word(board), word, "{board}");
        }
    }

    /// **A BOARD's OWN DEBUGGER DECIDES**, from its facts: an EDBG, an ST-LINK or a DAPLink whose
    /// serial bridge rides a stated UART gives `probe`; a USB-serial bridge that is no debugger, a
    /// chip's own USB, native USB alone, and a debugger whose bridge states no UART give `memory`.
    #[test]
    fn a_boards_own_debugger_decides_its_default() {
        for (board, word) in [
            ("microchip-samd21-xpro", "probe"),
            ("st-nucleo-l476rg", "probe"),
            ("arduino-due", "memory"),
            ("esp-esp32c6-devkitc", "memory"),
            ("adafruit-feather-m0-express", "memory"),
            ("microchip-sam4e-xpro", "memory"),
        ] {
            assert_eq!(default_word(board), word, "{board}");
        }
    }

    /// **THE micro:bits TAKE THE RING UNTIL AN IMAGE CAN WRITE THEIR DEBUGGER's UART.**
    #[test]
    fn the_micro_bits_take_the_ring_until_their_uart_is_written() {
        let v1 = load_board("bbc-micro-bit-v1").expect("the v1's board.toml");
        assert!(debugger_carries_a_uart(&v1), "the v1's facts state its DAPLink's UART");
        assert_eq!(console_default(&v1), ConsoleChannel::Memory);
        assert_eq!(default_word("bbc-micro-bit-v2"), "memory");
    }

    /// Every board in the catalog has a default, and only `memory` and `probe` are given.
    #[test]
    fn every_board_has_a_default() {
        let mut probe = 0;
        for (id, _) in BOARDS {
            if default_word(id) == "probe" {
                probe += 1;
            }
        }
        assert!(probe >= 2 && probe < BOARDS.len(), "{probe} of {} boards default to probe", BOARDS.len());
    }

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
    /// makes a value predictable. So the count moves as probes read more parts rather than with
    /// edits here, and a ratchet over it would gate on hardware access rather than on the code.
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

    /// Every embedded bus table is the one bsp-gen derives from the board's facts today, and only a
    /// board whose family has a table carries one. A generated file left stale, or a catalog built
    /// before it was regenerated, fails here by board.
    #[test]
    fn every_bus_table_is_the_one_bsp_gen_derives() {
        use lamella_bsp_gen::strata::{BUS_TABLE_FAMILIES, bus_table as derive, load_family, resolve_board};
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        let mut sets = std::collections::BTreeMap::new();
        let mut tables = 0;
        for (id, _) in BOARDS {
            let board = load_board(id).unwrap_or_else(|| panic!("{id}: board.toml parses"));
            let family = family_of(&board).unwrap_or_else(|| panic!("{id}: its family resolves"));
            if !BUS_TABLE_FAMILIES.contains(&family.as_str()) {
                assert_eq!(bus_table(id), None, "{id}: a {family} board carries a bus table");
                continue;
            }
            let set = sets
                .entry(family.clone())
                .or_insert_with(|| load_family(&repo, &family).unwrap_or_else(|e| panic!("{family}: {e}")));
            let resolved = resolve_board(set, board).unwrap_or_else(|e| panic!("{id}: {e}"));
            let derived = derive(set, &resolved).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(bus_table(id), Some(derived.as_slice()), "{id}: the embedded table is stale; regenerate");
            tables += 1;
        }
        assert_eq!(tables, BUS_TABLES.len(), "every embedded table belongs to a board of a table family");
        assert!(tables >= 4, "the four RP2350 boards carry tables");
    }

    /// A module-carrying board's family is its module's, as its part is.
    #[test]
    fn a_module_carrying_boards_family_is_its_modules() {
        let board = load_board("arduino-mkr1000").expect("the mkr1000 board file");
        assert_eq!(family_of(&board).as_deref(), Some("samd21"));
        assert_eq!(family_of(&load_board("rpi-pico2").expect("pico2")).as_deref(), Some("rp2350"));
    }

    /// The two ways a board id fails must not read alike.
    #[test]
    fn an_unknown_id_and_an_unresolvable_board_report_differently() {
        let unknown = resolve("no-such-board").expect_err("refuses");
        assert!(unknown.contains("lamella boards"), "it says how to look: {unknown}");
        assert!(resolve("rpi-pico").is_ok(), "a real board resolves");
    }
}
