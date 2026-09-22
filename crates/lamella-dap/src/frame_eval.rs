//! Reading a variable out of the frame execution is paused in: what answers a hover, a watch,
//! and a Debug Console line the client scoped to a frame.

use lamella_debug_backend::Variable;

/// Where one `evaluate` submission is to be answered from.
pub enum Target {
    /// The paused frame, counted as `DebugBackend::stack` lists them (0 is innermost).
    Frame {
        /// Which frame the answer is read from.
        index: usize,
        /// How much room the client gives the answer.
        room: Room,
    },
    /// The debugger's own session, which has no frame: a plain Debug Console line typed while
    /// nothing is paused.
    Session,
}

/// How much room the client gives an answer, which decides how long a refusal may be.
#[derive(Clone, Copy)]
pub enum Room {
    /// Rendered in a single cell -- a Watch row, or a hover popup -- and re-rendered at every
    /// stop, so the answer is a few words.
    Inline,
    /// Written to the Debug Console, which has room for a sentence saying what to do instead.
    Console,
}

/// Decides where a submission belongs from the two fields that say so: the `frameId` the client
/// sent, if any, and the `context` it gave.
///
/// A submission is scoped to a frame when the client names one, and also when its context is
/// `watch` or `hover` -- those are scoped by their nature, and a client that omits `frameId` on
/// them means the innermost frame, which is where the user is looking.
#[must_use]
pub fn route(frame_id: Option<u32>, context: &str) -> Target {
    let room = if matches!(context, "watch" | "hover") { Room::Inline } else { Room::Console };
    match (frame_id, context) {
        (Some(index), _) => Target::Frame { index: index as usize, room },
        (None, "watch" | "hover") => Target::Frame { index: 0, room },
        (None, _) => Target::Session,
    }
}

/// Why a frame could not answer a submission. Each variant is a different thing to do about it,
/// which is the reason they are told apart at all: one is the expression's shape, one is the
/// target's debug information, and one is the name itself.
#[derive(Debug)]
pub enum Refusal {
    /// The submission is not a single variable name, which is all a frame is read by.
    NotAName,
    /// The frame reports no arguments and no locals -- so no name could be found in it, and the
    /// submission is not what is wrong.
    NoVariables,
    /// The frame has variables and this name is not one of them.
    Absent,
}

/// Reads `expression` out of a frame's `visible` variables (its arguments and locals).
///
/// Only a single variable's name is read. An expression over a frame's values is deliberately not
/// evaluated here, and deliberately not passed on to the debugger's own session either: that
/// session has never seen the target and cannot reach the frame, so a name from the frame would
/// either fail there or -- worse -- resolve to something else of the same name that the session
/// does know. **A watch showing a value from the wrong place is worse than a watch showing none**,
/// because nothing about the value says where it came from.
///
/// # Errors
/// [`Refusal`], saying which of the three reasons it was.
pub fn resolve<'v>(visible: &'v [Variable], expression: &str) -> Result<&'v Variable, Refusal> {
    let name = expression.trim();
    if !is_name(name) {
        return Err(Refusal::NotAName);
    }
    if visible.is_empty() {
        return Err(Refusal::NoVariables);
    }
    visible.iter().find(|variable| variable.name == name).ok_or(Refusal::Absent)
}

/// Whether `text` is one C# identifier: a letter or underscore, then letters, digits and
/// underscores. Unicode letters count, as they do in the language.
fn is_name(text: &str) -> bool {
    let mut characters = text.chars();
    characters.next().is_some_and(|first| first.is_alphabetic() || first == '_')
        && characters.all(|rest| rest.is_alphanumeric() || rest == '_')
}

impl Refusal {
    /// What to tell the user, in the room the client gives the answer. `name` is the submission
    /// as typed, echoed by the reasons that are about a particular name.
    #[must_use]
    pub fn message(&self, name: &str, room: Room) -> String {
        match (self, room) {
            (Refusal::NotAName, Room::Inline) => "<only a variable name>".to_owned(),
            (Refusal::NotAName, Room::Console) => "Only a variable's name can be read from a \
                 paused frame. Anything longer is evaluated in a session of its own, which cannot \
                 see that frame."
                .to_owned(),
            (Refusal::NoVariables, Room::Inline) => "<target reports no variables>".to_owned(),
            (Refusal::NoVariables, Room::Console) => format!(
                "This target reports no arguments and no locals for the frame, so there is \
                 nothing to read {name} from. A target whose debug information carries no \
                 variable locations reports none."
            ),
            (Refusal::Absent, Room::Inline) => "<not in this frame>".to_owned(),
            (Refusal::Absent, Room::Console) => {
                format!("{name} is not an argument or a local of the paused frame.")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variable(name: &str, value: &str, kind: &str) -> Variable {
        Variable { name: name.to_owned(), value: value.to_owned(), kind: kind.to_owned() }
    }

    fn frame() -> Vec<Variable> {
        vec![variable("count", "7", "int"), variable("name", "\"ada\"", "string")]
    }

    fn scoped(target: &Target) -> Option<usize> {
        match target {
            Target::Frame { index, .. } => Some(*index),
            Target::Session => None,
        }
    }

    #[test]
    fn a_frame_scoped_submission_routes_to_the_frame_and_an_unscoped_one_to_the_session() {
        assert_eq!(scoped(&route(Some(2), "repl")), Some(2), "a frameId scopes it");
        assert_eq!(scoped(&route(None, "watch")), Some(0), "a watch is scoped by its nature");
        assert_eq!(scoped(&route(None, "hover")), Some(0), "so is a hover");
        assert_eq!(scoped(&route(None, "repl")), None, "a plain console line is not");
        assert_eq!(scoped(&route(None, "")), None, "nor one with no context at all");
    }

    #[test]
    fn frame_zero_is_a_frame_and_not_an_absent_frame_id() {
        assert_eq!(scoped(&route(Some(0), "repl")), Some(0));
        assert_eq!(scoped(&route(None, "repl")), None);
    }

    #[test]
    fn a_name_in_the_frame_resolves_to_its_value_and_its_type() {
        let visible = frame();
        let found = resolve(&visible, "count").expect("count is a local");
        assert_eq!(found.value, "7");
        assert_eq!(found.kind, "int");
    }

    #[test]
    fn surrounding_space_does_not_hide_a_name() {
        let visible = frame();
        assert!(resolve(&visible, "  count  ").is_ok());
    }

    #[test]
    fn the_three_reasons_a_frame_cannot_answer_are_told_apart() {
        let visible = frame();
        assert!(
            matches!(resolve(&visible, "missing"), Err(Refusal::Absent)),
            "a name the frame does not have"
        );
        assert!(
            matches!(resolve(&[], "count"), Err(Refusal::NoVariables)),
            "a frame that reports nothing at all"
        );
        assert!(
            matches!(resolve(&visible, "count + 1"), Err(Refusal::NotAName)),
            "an expression, which is not read from a frame"
        );
    }

    #[test]
    fn an_expression_is_refused_for_its_shape_even_when_the_frame_is_empty() {
        assert!(matches!(resolve(&[], "count + 1"), Err(Refusal::NotAName)));
    }

    #[test]
    fn what_counts_as_a_name() {
        let visible = frame();
        for named in ["count", "_x", "x1", "arg0", "this", "Ünicode"] {
            let answer = resolve(&visible, named);
            assert!(
                !matches!(answer, Err(Refusal::NotAName)),
                "{named} is a C# identifier"
            );
        }
        for not_named in ["count + 1", "p.X", "a[0]", "", "1x", "count()", "-count"] {
            assert!(
                matches!(resolve(&visible, not_named), Err(Refusal::NotAName)),
                "{not_named} is not one identifier"
            );
        }
    }

    #[test]
    fn an_inline_answer_is_a_short_marker_and_a_console_answer_is_prose() {
        for refusal in [Refusal::NotAName, Refusal::NoVariables, Refusal::Absent] {
            let inline = refusal.message("count", Room::Inline);
            let console = refusal.message("count", Room::Console);
            assert!(inline.len() < 40, "an inline answer is short: {inline}");
            assert!(
                inline.starts_with('<') && inline.ends_with('>'),
                "an inline answer is marked as a note, not shown as a value: {inline}"
            );
            assert!(!console.starts_with('<'), "a console answer is prose: {console}");
            assert!(console.ends_with('.'), "and a whole sentence: {console}");
            assert!(console.len() > inline.len(), "with more in it than the cell form");
        }
    }

    #[test]
    fn each_reason_reads_differently_in_both_rooms() {
        for room in [Room::Inline, Room::Console] {
            let said: Vec<String> = [Refusal::NotAName, Refusal::NoVariables, Refusal::Absent]
                .iter()
                .map(|refusal| refusal.message("count", room))
                .collect();
            for (first, second) in [(0, 1), (0, 2), (1, 2)] {
                assert_ne!(said[first], said[second], "two reasons read alike: {said:?}");
            }
        }
    }

    #[test]
    fn a_message_reads_as_one_line_of_prose() {
        for refusal in [Refusal::NotAName, Refusal::NoVariables, Refusal::Absent] {
            for room in [Room::Inline, Room::Console] {
                let said = refusal.message("count", room);
                assert!(!said.contains('\n'), "a message is one line: {said:?}");
                assert!(!said.contains("  "), "with single spaces between words: {said:?}");
                assert_eq!(said.trim(), said, "and no edges: {said:?}");
            }
        }
    }

    #[test]
    fn the_console_answer_names_the_variable_it_is_about() {
        assert!(Refusal::Absent.message("count", Room::Console).contains("count"));
        assert!(Refusal::NoVariables.message("count", Room::Console).contains("count"));
        assert!(!Refusal::NotAName.message("count + 1", Room::Console).contains("count + 1"));
    }
}
