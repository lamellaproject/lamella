//! How `lamella run` ends, whatever it ran and wherever it ran it.
//!
//! A program that returned, or exited, with a code gives that code back unchanged, in every
//! language and every mode, as `dotnet run`, `python` and `node` each do. Every other ending is 1,
//! with one sentence on standard error: this tool's own failure, an exception nothing caught, and a
//! run stopped by a time limit its caller set. As with those three runners, a caller tells this
//! tool's 1 from a program's own 1 by that sentence.
//!
//! Ctrl+C is the one exception. A run it stops ends the way the operating system ends any program
//! stopped by Ctrl+C, so a shell, or a script running one program after another, sees what it sees
//! for every other program: on Windows the exit code `0xC000013A`, and on Linux and macOS death by
//! `SIGINT`, which a shell reports as 130. A program on a board keeps running; the tool only lets go
//! of the board.

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The program returned, or exited, with this code.
    Program(i32),
    /// The program ended without returning a code of its own: an exception nothing caught stopped
    /// it, or, on a board, it was aborted.
    Uncaught,
    /// The time limit the caller set ran out before the program ended.
    TimedOut,
    /// This tool could not do what it was asked, and has said why on standard error.
    Failed,
    /// Ctrl+C stopped the run.
    Interrupted,
}

/// The exit code Windows gives a program Ctrl+C stopped, `STATUS_CONTROL_C_EXIT`.
#[cfg(windows)]
const INTERRUPTED: i32 = windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT;

/// What a shell reports for a program that `SIGINT` ended: 128 plus the signal's number, 2.
#[cfg(not(windows))]
const INTERRUPTED: i32 = 128 + 2;

impl Ended {
    /// The exit code a caller reads.
    ///
    /// The program's own code, unchanged; 1 for every other ending but Ctrl+C; and for Ctrl+C,
    /// what this operating system reports for any program it stopped. On Linux and macOS that is
    /// how a shell reports a death by signal, since [`Ended::finish`] ends the process by the
    /// signal rather than by an exit code.
    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            Ended::Program(code) => code,
            Ended::Uncaught | Ended::TimedOut | Ended::Failed => 1,
            Ended::Interrupted => INTERRUPTED,
        }
    }

    /// Ends this process as the run ended, or returns the code `main` ends it with.
    ///
    /// Windows keeps a code's 32 bits, so a code outside 0 to 255 is given back whole by ending the
    /// process here, after both streams are flushed; Linux and macOS keep a code's low 8 bits, for
    /// this program as for any other.
    #[must_use]
    pub fn finish(self) -> ExitCode {
        if self == Ended::Interrupted {
            end_as_interrupted();
        }
        let code = self.code();
        match u8::try_from(code) {
            Ok(small) => ExitCode::from(small),
            Err(_) => exit_whole(code),
        }
    }
}

/// A code `ExitCode` cannot carry, given back whole: Windows keeps all of it.
#[cfg(windows)]
fn exit_whole(code: i32) -> ExitCode {
    flush_both();
    std::process::exit(code)
}

/// A code `ExitCode` cannot carry: these systems keep its low 8 bits, so that is what is returned.
#[cfg(not(windows))]
fn exit_whole(code: i32) -> ExitCode {
    ExitCode::from(code.to_le_bytes()[0])
}

/// Whether Ctrl+C has been pressed since [`watch_for_ctrl_c`].
static PRESSED: AtomicBool = AtomicBool::new(false);

/// From here on, Ctrl+C is noticed rather than obeyed at once: [`ctrl_c_pressed`] reports it, so a
/// run can let go of its board before it ends as [`Ended::Interrupted`].
///
/// Returns whether it is watched. It is not where the operating system would not arrange it, and
/// not where Ctrl+C was already ignored when this program started, as it is for a program a shell
/// runs in the background: that is left as it was, and Ctrl+C keeps meaning nothing.
pub fn watch_for_ctrl_c() -> bool {
    platform::watch()
}

/// Whether Ctrl+C has been pressed since [`watch_for_ctrl_c`].
#[must_use]
pub fn ctrl_c_pressed() -> bool {
    PRESSED.load(Ordering::SeqCst)
}

/// A local run's interrupt watcher, ended and joined when the run returns normally.
pub(crate) struct LocalRunWatch(Option<(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>)>);

impl Drop for LocalRunWatch {
    fn drop(&mut self) {
        if let Some((stop, watcher)) = self.0.take() {
            let _ = stop.send(());
            let _ = watcher.join();
            stop_local_run_if_pressed();
        }
    }
}

/// Says `Stopped.` and ends a local run on Ctrl+C, including while its interpreter is busy.
/// The signal handler only sets a flag; the watcher writes and exits from an ordinary thread.
pub(crate) fn watch_local_run() -> LocalRunWatch {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    let (stop, stopped) = channel();
    let watcher = std::thread::Builder::new().name("lamella-interrupt".to_owned()).spawn(move || {
        loop {
            let finished = matches!(stopped.recv_timeout(std::time::Duration::from_millis(10)),
                Ok(()) | Err(RecvTimeoutError::Disconnected));
            stop_local_run_if_pressed();
            if finished { return; }
        }
    });
    let Ok(watcher) = watcher else {
        return LocalRunWatch(None);
    };
    let watch = LocalRunWatch(Some((stop, watcher)));
    if watch_for_ctrl_c() {
        watch
    } else {
        drop(watch);
        LocalRunWatch(None)
    }
}

fn stop_local_run_if_pressed() {
    if ctrl_c_pressed() {
        crate::own_lines::say(&mut std::io::stderr(), "Stopped.");
        end_as_interrupted();
    }
}

/// Ends this process the way the operating system ends a program Ctrl+C stopped.
fn end_as_interrupted() -> ! {
    flush_both();
    platform::end_as_interrupted()
}

/// Flushes both standard streams, so nothing a run printed is lost when the process ends without
/// returning from `main`.
fn flush_both() {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use super::{INTERRUPTED, Ordering, PRESSED};
    use windows_sys::Win32::Foundation::{FALSE, TRUE};
    use windows_sys::Win32::System::Console::{CTRL_C_EVENT, SetConsoleCtrlHandler};
    use windows_sys::core::BOOL;

    /// Called, on a thread of the system's, for each control event the console delivers.
    ///
    /// Ctrl+C is taken and remembered. Every other event goes on to the next handler, and in the
    /// end to the system's own, which ends the process as it would without this one.
    unsafe extern "system" fn on_control(event: u32) -> BOOL {
        if event == CTRL_C_EVENT {
            PRESSED.store(true, Ordering::SeqCst);
            return TRUE;
        }
        FALSE
    }

    pub(super) fn watch() -> bool {
        unsafe { SetConsoleCtrlHandler(Some(on_control), TRUE) != 0 }
    }

    pub(super) fn end_as_interrupted() -> ! {
        std::process::exit(INTERRUPTED)
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
mod platform {
    use super::{INTERRUPTED, Ordering, PRESSED};

    /// The `SIGINT` handler: it only records the press, which is all a handler may safely do.
    extern "C" fn on_interrupt(_signal: libc::c_int) {
        PRESSED.store(true, Ordering::SeqCst);
    }

    pub(super) fn watch() -> bool {
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGINT, std::ptr::null(), &mut current) != 0 {
                return false;
            }
            if current.sa_sigaction == libc::SIG_IGN {
                return false;
            }
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = libc::SA_RESTART;
            libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) == 0
        }
    }

    pub(super) fn end_as_interrupted() -> ! {
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::raise(libc::SIGINT);
        }
        std::process::exit(INTERRUPTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **THE PROGRAM'S OWN CODE COMES BACK UNCHANGED, AND EVERYTHING ELSE IS 1** -- a 3, a 70 (which
    /// a C# program may return itself), a negative code and one past 255 included.
    #[test]
    fn a_programs_code_comes_back_unchanged_and_every_other_ending_is_one() {
        for code in [0, 1, 3, 70, 255, 256, -1, i32::MIN] {
            assert_eq!(Ended::Program(code).code(), code);
        }
        for other in [Ended::Uncaught, Ended::TimedOut, Ended::Failed] {
            assert_eq!(other.code(), 1, "{other:?}");
        }
    }

    /// **CTRL+C GIVES THE CODE THIS SYSTEM GIVES EVERY PROGRAM IT STOPPED**: the code Windows
    /// reports for `ping`, `python` and `node` alike, and what a shell reports for `SIGINT`.
    #[test]
    fn ctrl_c_gives_the_code_this_system_gives_any_program_it_stopped() {
        #[cfg(windows)]
        assert_eq!(Ended::Interrupted.code() as u32, 0xC000_013A);
        #[cfg(not(windows))]
        assert_eq!(Ended::Interrupted.code(), 130);
    }

    /// The codes `main` returns itself: a byte as it stands, and 1 for every ending of this tool's.
    #[test]
    fn a_code_that_fits_a_byte_is_returned_from_main() {
        let shown = |ended: Ended| format!("{:?}", ended.finish());
        assert_eq!(shown(Ended::Program(0)), format!("{:?}", ExitCode::SUCCESS));
        assert_eq!(shown(Ended::Program(3)), format!("{:?}", ExitCode::from(3)));
        assert_eq!(shown(Ended::Program(70)), format!("{:?}", ExitCode::from(70)));
        for other in [Ended::Uncaught, Ended::TimedOut, Ended::Failed] {
            assert_eq!(shown(other), format!("{:?}", ExitCode::FAILURE), "{other:?}");
        }
    }

    /// Nothing is pressed until somebody presses it.
    #[test]
    fn ctrl_c_is_not_pressed_until_it_is() {
        assert!(!ctrl_c_pressed());
    }

    #[test]
    fn pending_ctrl_c_at_local_exit_is_not_discarded() {
        const CHILD: &str = "LAMELLA_LOCAL_EXIT_TEST";
        if std::env::var_os(CHILD).is_some() {
            let (stop, stopped) = std::sync::mpsc::channel();
            let watcher = std::thread::spawn(move || { stopped.recv().unwrap(); });
            let watch = LocalRunWatch(Some((stop, watcher)));
            PRESSED.store(true, Ordering::SeqCst);
            println!("local-exit-boundary-marker");
            drop(watch);
            println!("local-exit-swallowed-marker");
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "ending::tests::pending_ctrl_c_at_local_exit_is_not_discarded", "--nocapture"])
            .env(CHILD, "1").stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
            .spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the local exit helper did not finish");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("local-exit-boundary-marker"));
        #[cfg(windows)]
        assert_eq!(output.status.code(), Some(INTERRUPTED));
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(output.status.signal(), Some(libc::SIGINT));
        }
        assert_eq!(output.stderr, b"Stopped.\n");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("local-exit-swallowed-marker"));
    }
}
