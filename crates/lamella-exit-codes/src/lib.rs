//! The codes a run reports when its program ends any way other than its entry point returning, and
//! what each one means.
//!
//! A program that returns from its entry point reports what it returned, or 0 for an entry point
//! that returns nothing. A run that ends any other way reports one of these, so a host can tell
//! from the code alone what ended it: an exception the program did not catch, the runtime stopping
//! the program itself, or the processor faulting. A Lamella RHU board's kernel gives one more code,
//! when it refuses to start the program.
//!
//! [`meaning`] says what each code means in plain words, so every tool that reports how a program
//! ended says it the same way.
#![cfg_attr(not(test), no_std)]

/// The program ended on an exception it did not catch: 134, the code .NET reports for one on
/// Linux and macOS.
pub const UNHANDLED_EXCEPTION: i32 = 134;

/// The runtime stopped the program itself: an instruction it does not run, an operation it
/// refuses, an image that does not boot, a firmware that panicked. The program raised no
/// exception, and the processor did not fault.
pub const FATAL_STOP: i32 = 1;

/// The processor faulted: it refused an access or an instruction, and the firmware's fault
/// handler ended the run.
pub const PROCESSOR_FAULT: i32 = 2;

/// A Lamella RHU board's kernel refused to start the program. The kernel gives this code and no
/// Lamella runtime does: a run that a runtime started never reports it.
pub const KERNEL_REFUSED_ENTRY: i32 = 4;

/// What `code` means for a program that did not return, in plain words: a clause that finishes a
/// sentence about the program, as in "The program ended with code 2: the processor faulted."
///
/// `None` for a code with no meaning here, which a host names by its number alone.
#[must_use]
pub const fn meaning(code: i32) -> Option<&'static str> {
    match code {
        UNHANDLED_EXCEPTION => Some("an exception nothing caught, a fail-fast or an abort ended it"),
        FATAL_STOP => Some("the runtime stopped it on a fatal error of its own"),
        PROCESSOR_FAULT => Some("the processor faulted"),
        KERNEL_REFUSED_ENTRY => Some("a Lamella RHU board's kernel refused to start it"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{FATAL_STOP, KERNEL_REFUSED_ENTRY, PROCESSOR_FAULT, UNHANDLED_EXCEPTION, meaning};

    #[test]
    fn each_code_says_what_ended_the_program_in_plain_words() {
        assert_eq!(
            meaning(UNHANDLED_EXCEPTION),
            Some("an exception nothing caught, a fail-fast or an abort ended it")
        );
        assert_eq!(meaning(FATAL_STOP), Some("the runtime stopped it on a fatal error of its own"));
        assert_eq!(meaning(PROCESSOR_FAULT), Some("the processor faulted"));
        assert_eq!(meaning(KERNEL_REFUSED_ENTRY), Some("a Lamella RHU board's kernel refused to start it"));
    }

    #[test]
    fn a_code_this_table_does_not_define_has_no_meaning() {
        for code in [0, 3, 5, 70, 255, -1, i32::MIN] {
            assert_eq!(meaning(code), None, "code {code}");
        }
    }

    #[test]
    fn no_two_codes_are_the_same() {
        let codes = [UNHANDLED_EXCEPTION, FATAL_STOP, PROCESSOR_FAULT, KERNEL_REFUSED_ENTRY];
        for (index, code) in codes.iter().enumerate() {
            assert!(!codes[index + 1..].contains(code), "code {code} appears twice");
        }
        assert_eq!(codes, [134, 1, 2, 4]);
    }
}
