//! A board's interpreter, over Lamella Link: the image the build baked for the board, written to it
//! and started halted in one connection, debugged at source level through the build's line map.

use crate::launch::Plan;
use lamella_dap::DebugBackend;
use lamella_wire_host::debug_backend::WireHostBackend;
use std::time::Duration;

/// The rate a serial carrier is opened at. A board's own USB ignores it.
const BAUD: u32 = 115_200;

/// How long a board has to answer each question before the session says it did not.
const ANSWER: Duration = Duration::from_secs(5);

/// The backend for a program on the board `target` names: the board reached and asked what it is,
/// refused unless it is the board the image was built for, and given the image and its line map.
/// The image is written when the session launches, not here.
///
/// # Errors
/// Why the program cannot be debugged, in words for the user: a file that cannot be read, a board
/// that cannot be reached or does not answer, or a board other than the one the build was for.
pub fn backend(plan: &Plan, target: &str) -> Result<Box<dyn DebugBackend>, String> {
    let image_path = plan.image.display();
    let image = std::fs::read(&plan.image)
        .map_err(|error| format!("could not read the image {image_path}: {error}"))?;
    let line_map = match &plan.line_map {
        Some(path) => Some(std::fs::read(path).map_err(|error| {
            format!("could not read the file that maps the program to its source, {}: {error}", path.display())
        })?),
        None => None,
    };
    let backend = WireHostBackend::open_target(target, BAUD, image, ANSWER).map_err(|failure| {
        format!(
            "could not reach the device {target}: {}",
            lamella_wire_host::open_failure(failure)
        )
    })?;
    serve(plan, target, backend, line_map)
}

/// A board that answered its HELLO, as [`serve`] decides what to do with it.
trait Answered {
    /// The product model the board answered as.
    fn product_model(&self) -> u16;
    /// The session's backend over this board, with the line map. **The one step after which a
    /// launch can write the image**: the backend this returns writes it when the session launches.
    fn served(self, line_map: Option<Vec<u8>>) -> Box<dyn DebugBackend>;
}

impl Answered for WireHostBackend {
    fn product_model(&self) -> u16 {
        self.identity().product_model
    }
    fn served(self, line_map: Option<Vec<u8>>) -> Box<dyn DebugBackend> {
        Box::new(self.with_srcmap(line_map))
    }
}

/// The session's backend over `answered`, unless it answered as another board than the one the
/// image was built for, which is refused before the backend that writes the image is made.
fn serve(
    plan: &Plan,
    target: &str,
    answered: impl Answered,
    line_map: Option<Vec<u8>>,
) -> Result<Box<dyn DebugBackend>, String> {
    if let Some(built_for) = &plan.board {
        refuse_another_board(built_for, answered.product_model(), target)?;
    }
    Ok(answered.served(line_map))
}

/// Refuses a board that answers as anything but `built_for`, the board the image was baked for.
///
/// **NOTHING HAS BEEN WRITTEN WHEN THIS RUNS.** An image baked for one board carries that board's bus
/// table and its family's build of the class library, so on another board it runs against pins and
/// libraries it was not built for -- and a board of the same chip family boots it without a word.
fn refuse_another_board(built_for: &str, answered_model: u16, target: &str) -> Result<(), String> {
    match lamella_wire_host::boards::board_answering(answered_model) {
        Some(answered) if answered.eq_ignore_ascii_case(built_for) => Ok(()),
        Some(answered) => Err(format!(
            "the device {target} is a {answered}, and the program was built for a {built_for}. \
             Nothing was written. Build the project for the board you debug on."
        )),
        None => Err(format!(
            "the device {target} answers as no board this debugger knows (product model \
             {answered_model:#06x}), and the program was built for a {built_for}. Nothing was \
             written. Write the Lamella firmware that names the board, or build for the board it is."
        )),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::launch::Device;
    use lamella_dap::{Disassembled, Frame, Register, Scope, Stop, Variable};
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::rc::Rc;

    /// The product model the catalog gives `board`.
    fn model_of(board: &str) -> u16 {
        let row = lamella_catalog::load_board(board).expect("a catalog board");
        u16::try_from(row.product_model).expect("a declared model")
    }

    /// A board that answered its HELLO as `model`, recording whether the session's backend was made
    /// over it -- the backend whose launch writes the image.
    struct Board {
        model: u16,
        served: Rc<Cell<bool>>,
    }

    impl Answered for Board {
        fn product_model(&self) -> u16 {
            self.model
        }
        fn served(self, _line_map: Option<Vec<u8>>) -> Box<dyn DebugBackend> {
            self.served.set(true);
            Box::new(NeverDriven)
        }
    }

    /// The backend a served [`Board`] hands back. These tests decide whether a board is served and
    /// never drive the session.
    pub(crate) struct NeverDriven;

    impl DebugBackend for NeverDriven {
        fn launch(&mut self) -> Result<(), String> {
            unreachable!("these tests start no session")
        }
        fn resume(&mut self) -> Stop {
            unreachable!()
        }
        fn step(&mut self) -> Stop {
            unreachable!()
        }
        fn depth(&self) -> usize {
            unreachable!()
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            unreachable!()
        }
        fn stack(&self) -> Vec<Frame> {
            unreachable!()
        }
        fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
            unreachable!()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            unreachable!()
        }
        fn read_registers(&self) -> Vec<Register> {
            unreachable!()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            unreachable!()
        }
        fn take_output(&mut self) -> Option<String> {
            unreachable!()
        }
    }

    /// The plan of a launch whose image was built for `board`, or for no board named.
    fn built_for(board: Option<&str>) -> Plan {
        Plan {
            device: Device::Link("usb:x".to_owned()),
            image: PathBuf::from("App.lmli"),
            symbols: None,
            line_map: None,
            board: board.map(str::to_owned),
            references: None,
            stop_at_entry: false,
            no_debug: false,
            attach: false,
            args: Vec::new(),
            path_map: lamella_dap::PathMap::default(),
            notes: Vec::new(),
        }
    }

    /// `board`'s HELLO answering as `model`, served or refused for `plan`: what it was told, and
    /// whether the backend that writes the image was made over it.
    fn answered_as(plan: &Plan, model: u16) -> (Result<(), String>, bool) {
        let served = Rc::new(Cell::new(false));
        let board = Board { model, served: served.clone() };
        let outcome = serve(plan, "usb:x", board, None).map(|_| ());
        (outcome, served.get())
    }

    #[test]
    fn the_board_the_image_was_built_for_is_served() {
        let plan = built_for(Some("raspberrypi-pico-2-w"));
        assert_eq!(answered_as(&plan, model_of("raspberrypi-pico-2-w")), (Ok(()), true));
        assert_eq!(answered_as(&built_for(None), model_of("raspberrypi-pico-2")), (Ok(()), true));
    }

    /// **NOTHING IS WRITTEN FOR ANOTHER BOARD**: the refusal comes before the backend whose launch
    /// writes the image is made, so no session ever holds one for the wrong board. Driven through
    /// [`serve`], the step `backend` takes after the HELLO, so a reordering that made the backend
    /// first fails here.
    #[test]
    fn another_board_is_refused_naming_both_before_anything_is_written() {
        let plan = built_for(Some("raspberrypi-pico-2-w"));
        let (outcome, served) = answered_as(&plan, model_of("raspberrypi-pico-2"));
        let reason = outcome.expect_err("a Pico 2 answered for a Pico 2 W's image");
        assert!(!served, "the backend that writes the image was made for another board: {reason}");
        assert!(
            reason.contains("raspberrypi-pico-2 ")
                || reason.contains("a raspberrypi-pico-2,"),
            "{reason}"
        );
        assert!(reason.contains("raspberrypi-pico-2-w") && reason.contains("Nothing was written"), "{reason}");
    }

    #[test]
    fn a_board_that_names_no_known_model_is_refused_saying_so() {
        let (outcome, served) = answered_as(&built_for(Some("raspberrypi-pico-2-w")), 0);
        let reason = outcome.expect_err("unknown");
        assert!(!served, "{reason}");
        assert!(reason.contains("no board this debugger knows") && reason.contains("raspberrypi-pico-2-w"), "{reason}");
    }
}
