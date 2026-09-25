//! `lamella run` and `lamella build`: a program, in whichever language it is written.

use crate::args::{self, Spec};
use lamella_catalog::{self as catalog, BOARD_PYTHON};
use lamella_bsp_gen::fit::fit;
use lamella_wire_host::engine::{
    LcscCompiler, LoopbackLink, Outcome, Repl, ReplError, ReplLink, install_host_clock,
};
use lamella_js_frontend::interpreter::{Completion, Interpreter};
use lamella_js_frontend::value::JsValue;
use std::path::Path;
use std::process::ExitCode;

/// A language this tool can be handed a file in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Language {
    CSharp,
    Python,
    JavaScript,
}

impl Language {
    /// The language `path`'s extension names.
    ///
    /// # Errors
    /// An extension no language claims. The message lists the ones that work AND where an image
    /// another toolchain has already produced goes, because a reader holding a `.swift`, a `.ts`
    /// or a `.rs` is asking two questions at once -- which files this verb takes, and where theirs
    /// is handled. Naming only the first answers half of it and reads as the second.
    fn of(path: &Path) -> Result<Language, String> {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("cs") => Ok(Language::CSharp),
            Some("py") => Ok(Language::Python),
            Some("js") => Ok(Language::JavaScript),
            _ => Err(format!(
                "{}: lamella run and lamella build read .cs (C#), .csproj (a C# project), .py (Python) and\n\
.js (ECMAScript, also known as JavaScript).\n\n\
An image another toolchain has already produced -- .elf, .bin, .hex or .s19 -- is written by\n\
`lamella flash <image> --board <id>`, which compiles nothing. A linked .elf is taken directly\n\
and flattened by physical address, so it needs no conversion step first.",
                path.display()
            )),
        }
    }
}

/// The redirect for `--board`, which this verb no longer has.
///
/// **IT SAYS WHERE THE FLAG WENT AND WHAT IT DID, WITHOUT IMPLYING IT REACHED HARDWARE.** A reader
/// meets this at the moment an invocation that worked stopped working, so "unknown option" would
/// be the wrong answer and so would silence about what they lose.
const BOTH_MODES: &str = "\
--board is no longer an option of this verb. It ran the program on THIS machine with a board's
generated `board` module on the import path -- a fact table, with nothing behind the addresses --
which is not what `run` means to a reader.

    (neither)         run it on this machine
    --target <t>      run it ON the board at <t>, with its output here";

const RUN_USAGE: &str = "\
usage: lamella run <file.cs|file.csproj|file.py|file.js> [--target <t>]

Compiles and runs the program, and STAYS until it ends -- its output appears here as it is
printed. A program written to loop forever runs until you stop this tool.

With neither option it runs on this machine, which needs no hardware and is the fastest way to
find out whether a program compiles and does what you meant.

A .csproj compiles every .cs beside it as one program, as `lamella build` compiles it. This
machine and firmware on a board both run a program against the class library alone, so a project
that references libraries of its own is built with `lamella build --class-library`, which links
them.

--target <t> runs a C# program -- a .cs or a .csproj -- ON a board that already has firmware, with
the output still appearing here. `lamella devices` prints the word to pass. A cycle is about a
second, and the board keeps its firmware. A Python program and a JavaScript program run on this
machine.

Two questions this verb does not answer: whether a program FITS a board is `build --board <id>`,
and putting it on one is `deploy`.";

/// `lamella run <file> [--board <id>]`: compile and run on this machine.
pub fn run_command(args: &[String]) -> ExitCode {
    let spec =
        Spec { verb: "run", usage: Some(RUN_USAGE), values: &["--board", "--target"], flags: &[] };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    let path = match parsed.only_positional("run", "source file") {
        Ok(path) => Path::new(path).to_path_buf(),
        Err(error) => {
            eprintln!("{error}\n\n{}", RUN_USAGE.lines().next().unwrap_or_default());
            return ExitCode::FAILURE;
        }
    };
    if parsed.value("--board").is_some() {
        eprintln!("lamella run: {BOTH_MODES}");
        return ExitCode::FAILURE;
    }
    if let Some(target) = parsed.value("--target") {
        return run_on_target(&path, target);
    }
    if crate::flash::is_project(&path) {
        return run_project(&path);
    }

    let (language, source) = match read(&path) {
        Ok(read) => read,
        Err(error) => {
            eprintln!("lamella run: {error}");
            return ExitCode::FAILURE;
        }
    };
    match language {
        Language::CSharp => run_csharp(&path, &source),
        Language::Python => run_python(&path, &source, None),
        Language::JavaScript => run_javascript(&path, &source),
    }
}

/// `lamella run <file.js>` on this machine.
///
/// # THE ENGINE IS A LANGUAGE, NOT A PLATFORM, AND THE HOST SEAMS FAIL SILENTLY
///
/// ECMA-262 defines no output at all -- no `console`, no `print` -- so a host that installs nothing
/// ships a verb whose programs cannot say anything. That is not an error and not a crash: the
/// program runs and produces nothing, which reads as a broken tool. The clock is the same shape,
/// where `Date.now()` would sit at the epoch and never advance.
///
/// **`print` and not `console.log`**: `console` is not in the standard either, and inventing that
/// namespace on the realm would put a browser's shape where the standard has none.
fn run_javascript(path: &Path, source: &str) -> ExitCode {
    let parsed = lamella_js_frontend::parse_script(source);
    if parsed.has_errors() {
        for diagnostic in parsed.diagnostics.iter().filter(|d| d.is_error()) {
            eprintln!("{}: {}", path.display(), diagnostic.message);
        }
        return ExitCode::FAILURE;
    }

    let mut interpreter = javascript_realm();
    interpreter.define_host_function("print", 1, |interpreter, _this, arguments| {
        let text = match arguments.first() {
            Some(value) => interpreter.describe(value),
            None => String::new(),
        };
        println!("{text}");
        Completion::Normal(JsValue::Undefined)
    });

    match interpreter.run_source(source) {
        Ok(Completion::Throw(value)) => {
            eprintln!("{}: uncaught {}", path.display(), interpreter.describe(&value));
            ExitCode::FAILURE
        }
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: {error}", path.display());
            ExitCode::FAILURE
        }
    }
}

/// The realm `lamella run <file.js>` executes in, minus output.
///
/// **A SEPARATE FUNCTION SO THE SEAMS CAN BE OBSERVED.** A seam that is never installed fails
/// silently by construction -- the program runs and simply behaves as though the host had nothing
/// to offer -- so what a test needs is the realm the verb actually builds, not a second one
/// assembled beside it that can agree with the code while the verb disagrees. `print` is the one
/// seam left out: it writes to THIS process's stdout, which a test cannot read back.
fn javascript_realm() -> Interpreter {
    let mut interpreter = Interpreter::new();
    interpreter.set_host_clock(Some(epoch_millis()), monotonic_millis);
    interpreter.set_host_entropy(seed());
    interpreter
}

/// Milliseconds since the Unix epoch: the ANCHOR `Date.now()` counts from.
fn epoch_millis() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |since| since.as_millis() as f64)
}

/// The elapsed-time source, which is a DIFFERENT clock from the anchor above and must be.
///
/// A wall clock can step backwards -- an NTP correction, a manual change -- so using it for elapsed
/// time lets `Date.now()` go down between two calls, and code that subtracts two readings gets a
/// negative duration. The engine anchors once and adds elapsed monotonic time, which is exactly why
/// it asks for the two separately.
fn monotonic_millis() -> f64 {
    host_monotonic_ns() as f64 / 1_000_000.0
}

/// The per-run seed for `Math.random`, which the engine takes ONCE rather than as a source.
///
/// **THE MONOTONIC SOURCE ABOVE CANNOT BE USED HERE**, and that is the trap this exists to avoid:
/// it counts from the first call in this process, so it reads near zero at start-up on every run
/// and would seed every run alike -- the exact failure a seed is for. The wall clock is read
/// instead, at nanosecond resolution, because what a seed needs is to DIFFER between runs and not
/// to move forwards within one.
///
/// **NOT FOR CRYPTOGRAPHY**, and the standard says so about `Math.random` itself. A clock is
/// guessable; anything that needs unguessable bits needs a different seam.
fn seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |since| since.as_nanos() as u64)
}

/// Run on a board in a build that can, and name the missing feature in one that cannot.
#[cfg(feature = "bake")]
fn run_on_target(path: &Path, target: &str) -> ExitCode {
    println!("{}", crate::attach::forever_warning());
    crate::attach::run_on_target(path, target)
}

#[cfg(not(feature = "bake"))]
fn run_on_target(_path: &Path, _target: &str) -> ExitCode {
    eprintln!(
        "lamella run: this build cannot run a program on a board.\n\n\
         Doing so bakes it into an image first, which needs the `bake` feature:\n\
         \x20   cargo build -p lamella-cli --features bake\n\n\
         Without a --target this build runs the program on THIS machine, which needs nothing."
    );
    ExitCode::FAILURE
}

/// Compile a C# program and run it on the host interpreter.
///
/// **THERE IS NO `--unsafe` HERE AND THAT IS NOT AN OVERSIGHT.** This verb runs the program on THIS
/// machine, where a raw pointer at a device register addresses host memory and means nothing --
/// so the switch would buy a program that compiles and then faults. A program written to drive
/// hardware belongs on `deploy`, which takes a source file and `--unsafe`, and the diagnostic below
/// says so where it comes up. **Not `flash`**, which takes an image rather than a source file and
/// declares no `--unsafe` at all.
fn run_csharp(path: &Path, source: &str) -> ExitCode {
    let compiler = match LcscCompiler::discover() {
        Ok(compiler) => compiler.for_source_file(&path.display().to_string()),
        Err(error) => {
            eprintln!("lamella run: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(corlib) = compiler.references().first().cloned() else {
        eprintln!("lamella run: the compiler found no reference assemblies");
        return ExitCode::FAILURE;
    };
    let mut repl = Repl::new(
        Box::new(compiler),
        Box::new(LoopbackLink::new(corlib, install_host_clock)),
    );
    report(repl.eval_program(source))
}

/// Compile a C# project and run it on the host interpreter, as a single source file is run.
///
/// **THE COMPILATION `build` MAKES OF IT** -- every `.cs` beside the project as one program, under
/// the project's own settings -- so a project checked here is the project a board gets. Only where
/// it runs differs.
///
/// **A PROJECT THAT NAMES LIBRARIES IS REFUSED, NOT RUN.** This machine runs a program against the
/// class library alone, so a name that resolves into a library of the project's own would load with
/// nothing to resolve to -- a failure that would point at the program rather than at this verb.
fn run_project(path: &Path) -> ExitCode {
    match project_outcome(path) {
        Ok(outcome) => report(outcome),
        Err(refusal) => {
            eprintln!("{refusal}");
            ExitCode::FAILURE
        }
    }
}

/// Compiles the project at `path` and runs it on this machine, or says why it will not -- the half
/// of [`run_project`] that decides, kept apart from the half that prints.
///
/// # Errors
/// A project that cannot be read or compiled, a class library, or a project naming libraries of its
/// own, each with the sentence that says so.
fn project_outcome(path: &Path) -> Result<Result<Outcome, ReplError>, String> {
    let project = program_project(
        path,
        "run",
        "This verb runs a program on this machine",
        &format!(
            "lamella build {} --board <id> --format <f> --class-library",
            path.display()
        ),
    )?;
    let (assembly, corlib) = compile_project_assembly(&project, &[], "run")?;
    let mut link = LoopbackLink::new(corlib, install_host_clock);
    Ok(link
        .run(1, &assembly)
        .map_err(ReplError::Transport)
        .map(|ran| Outcome::Ran {
            output: ran.stdout,
            exit: ran.exit,
            persisted: false,
        }))
}

/// The C# project at `path`, read as a program that runs against the class library alone, or why
/// it is not one.
///
/// **ONE RULE FOR EVERY ROUTE THAT RUNS A PROJECT WITHOUT LINKING IT** -- this machine, and firmware
/// already on a board. A class library has no entry point, and a project that names libraries of
/// its own would run with none of their code, so both are refused. Only the words differ: `runs`
/// says where the program would run, and `linked_build` is the command that builds it with its
/// libraries linked.
///
/// # Errors
/// A project that cannot be read, a class library, or a project naming libraries of its own.
pub(crate) fn program_project(
    path: &Path,
    verb: &str,
    runs: &str,
    linked_build: &str,
) -> Result<crate::project::Project, String> {
    let project = crate::project::Project::read_file(path, verb)?;
    if project.output_type == crate::project::OutputType::Library {
        return Err(format!(
            "lamella {verb}: {} builds a class library, which has no entry point, so there is \
             nothing to run.\n\n\
             A library runs as part of a program: name it in that program's project with a \
             <Reference>.",
            path.display()
        ));
    }
    if !project.references.is_empty() {
        let named: Vec<String> = project
            .references
            .iter()
            .map(|library| format!("    {}", library.display()))
            .collect();
        return Err(format!(
            "lamella {verb}: {} references libraries of its own:\n\n{}\n\n\
             {runs} against the class library alone, so a name that resolves into\none of those \
             would have nothing to resolve to. A build that links them is:\n\n\
             \x20   {linked_build}",
            path.display(),
            named.join("\n"),
        ));
    }
    Ok(project)
}

/// What `run` shows for a C# program, however it was compiled: its output, and -- when it did not
/// exit 0 -- the code it exited with and what that code can mean.
///
/// **ONE REPORTER FOR BOTH ROUTES**, so a source file and a project that run the same program are
/// reported the same way rather than by two copies that drift.
fn report(outcome: Result<Outcome, ReplError>) -> ExitCode {
    match outcome {
        Ok(Outcome::Ran { output, exit, .. }) => {
            print!("{output}");
            if exit == 0 {
                ExitCode::SUCCESS
            } else {
                eprintln!("lamella run: the program exited {exit}");
                eprint!("{}", exit_note(exit));
                ExitCode::FAILURE
            }
        }
        Ok(Outcome::CompileError(text)) => {
            eprintln!("{text}");
            if text.contains("CS0227") {
                eprintln!(
                    "\nunsafe code is off by default, as it is in csc without /unsafe -- and this \
                     verb has no switch\nfor it, because a raw pointer at a device register means \
                     nothing on this machine. A program that\ndrives hardware goes on a board:\n\
                     \x20   lamella deploy <file> --board <id> --unsafe\n\n\
                     Not `flash`: that verb writes an image somebody already built, so it\n\
                     takes no --unsafe -- by the time it runs, the compiling is over."
                );
            }
            ExitCode::FAILURE
        }
        Ok(Outcome::Empty) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lamella run: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Compile a Python program and run it on the host interpreter, optionally against a board's
/// generated `board` module.
fn run_python(path: &Path, source: &str, board: Option<&str>) -> ExitCode {
    let bundle = match compile_python(path, source, board) {
        Ok(bundle) => bundle,
        Err(error) => {
            eprintln!("lamella run: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut model = lamella_py_runtime::ObjectModel::new(Vec::new(), 16 * 1024 * 1024);
    model.set_clock(host_wall_ns, host_monotonic_ns, host_sleep_ns);
    if let Err(trap) = lamella_py_runtime::run_bundle(bundle, &mut model) {
        print!("{}", model.take_stdout());
        report_trap(&mut model, &trap);
        return ExitCode::FAILURE;
    }
    print!("{}", model.take_stdout());
    ExitCode::SUCCESS
}

/// Compile a Python program to a bundle: the entry module plus every module its imports reach.
///
/// An import resolves the way it does on the interpreter's own harnesses -- a sibling `.py` beside
/// the entry first, then the runtime's bundled module sources -- with one addition: a named board
/// serves that board's generated `board` module, from the copy compiled into this binary. A name
/// with none of those stays native and is served by the interpreter's built-in modules.
fn compile_python(
    path: &Path,
    source: &str,
    board: Option<&str>,
) -> Result<lamella_py_bytecode::Bundle, String> {
    let directory = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let board = board.map(str::to_owned);
    let resolve = move |name: &str| -> Option<String> {
        if name == "board"
            && let Some(id) = &board
        {
            return BOARD_PYTHON
                .iter()
                .find(|(board_id, _)| board_id == id)
                .map(|(_, text)| (*text).to_owned());
        }
        std::fs::read_to_string(directory.join(format!("{name}.py")))
            .ok()
            .or_else(|| lamella_py_runtime::pystdlib::bundled_module(name).map(String::from))
    };
    lamella_py_frontend::compile_bundle("__main__", source, &resolve)
        .map_err(|error| format!("compile {}: {error}", path.display()))
}

/// Report a trap: an uncaught Python exception by its type name, any other trap by its kind.
fn report_trap(model: &mut lamella_py_runtime::ObjectModel, trap: &lamella_py_runtime::Trap) {
    let pending = model.take_pending_exception();
    match pending.and_then(|exception| model.exception_type_name(exception).map(String::from)) {
        Some(name) => eprintln!("{name}"),
        None => eprintln!("{trap:?}"),
    }
}

/// Refuses every `#:` file-based-app directive this tool does not act on, by name.
///
/// **NOTHING IS ACCEPTED AND IGNORED, AND THAT IS THE WHOLE POINT OF THE FUNCTION.** The compiler
/// validates no directive name at all -- it lexes `#:` as a name plus the rest of the line and
/// hands both through untouched, so `#:pacakge Newtonsoft.Json` compiles clean and does nothing.
/// A program that builds while the dependency it asked for was silently dropped is a worse answer
/// than one that does not build, so a recognized-but-unhonored directive is refused with what it
/// asks for, exactly as an unrecognized one is.
///
/// # Errors
/// Any `#:` directive at all, today: none is honored yet. The two shapes are told apart because
/// they call for opposite next steps -- a misspelling is fixed by the author, an unhonored
/// directive is not.
fn refuse_unhonored_directives(
    source: &str,
    options: lamella_syntax::lexer::LexOptions,
) -> Result<(), String> {
    let tokenized = lamella_syntax::lexer::tokenize_with(source, options);
    if !tokenized.diagnostics.is_empty() {
        return Ok(());
    }
    let Some(directive) = tokenized.file_directives.first() else {
        return Ok(());
    };
    let asks_for = match &*directive.name {
        "package" => "a package by name, which needs a feed, version resolution and a restore step",
        "project" => "another project to reference, which needs a project system to resolve into",
        "include" => "another source file to compile with this one",
        "property" => "a build property",
        "sdk" => "an SDK to build against",
        other => return Err(format!("Unrecognized directive '{other}'.")),
    };
    Err(format!(
        "`#:{}` asks for {asks_for}, and this tool does not honor it.\n\n\
         It is refused rather than ignored: a program that built while the thing it asked for was \
         dropped\nwould run against something it did not ask for.",
        directive.name
    ))
}

/// `lamella board-module-run <file.py> --board <id>`.
///
/// **UNDOCUMENTED AND FOR THIS PROJECT'S OWN USE.** It is deliberately absent from `USAGE` and
/// from `lamella --help`, and the test beside the verb list asserts that absence so it cannot drift
/// back into the documented set by accident.
///
/// It runs the program on THIS machine with a board's generated `board` module on the import path.
/// That module is a fact table -- roles, pins, register addresses -- and nothing stands behind the
/// addresses, so what it answers is whether a program names a board's facts correctly, and never
/// what the hardware would do. **That is a check wearing a run-shaped verb, which is why it is no
/// longer one of `run`'s modes.**
///
/// **IT PRINTS NOTHING FOR A PROGRAM THAT DOES NOT TERMINATE**, which is most device programs:
/// output is buffered in the object model and flushed after the program returns, so a `while True:`
/// loop interrupted at the keyboard loses everything it printed.
pub fn board_module_run_command(args: &[String]) -> ExitCode {
    eprintln!(
        "lamella board-module-run: DEPRECATED and internal. It runs the program on this machine \
         with\n<id>'s generated `board` module on the import path -- a fact table, with nothing \
         behind the\naddresses. It answers whether a program NAMES a board's facts correctly and \
         nothing else.\nDo not build on it."
    );
    let spec = Spec { verb: "board-module-run", usage: None, values: &["--board"], flags: &[] };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    let path = match parsed.only_positional("board-module-run", "source file") {
        Ok(path) => Path::new(path).to_path_buf(),
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(id) = parsed.value("--board") else {
        eprintln!("lamella board-module-run: --board <id> is required; it is the whole point.");
        return ExitCode::FAILURE;
    };
    if let Err(error) = catalog::resolve(id) {
        eprintln!("lamella board-module-run: {error}");
        return ExitCode::FAILURE;
    }
    let (language, source) = match read(&path) {
        Ok(read) => read,
        Err(error) => {
            eprintln!("lamella board-module-run: {error}");
            return ExitCode::FAILURE;
        }
    };
    match language {
        Language::CSharp => {
            eprintln!(
                "lamella board-module-run: {} is C#. This path serves a generated `board` MODULE, \
                 which is Python's\nshape; a C# program reaches a board surface through a \
                 generated assembly it does not link.",
                path.display()
            );
            ExitCode::FAILURE
        }
        Language::Python => run_python(&path, &source, Some(id)),
        Language::JavaScript => {
            eprintln!(
                "lamella board-module-run: {} is JavaScript. This path serves a generated `board` \
                 MODULE, which is\nPython's shape; this JavaScript profile has no module loader \
                 at all, so nothing could import it.",
                path.display()
            );
            ExitCode::FAILURE
        }
    }
}

/// `lamella build <file> [--board <id>] [--out <path>]`: produce the artifact a device runs.
pub fn build_command(args: &[String]) -> ExitCode {
    let usage = usage();
    let spec = Spec {
        verb: "build",
        usage: Some(&usage),
        values: &["--board", "--out", "--format"],
        flags: &["--unsafe", crate::flash::CLASS_LIBRARY_FLAG],
    };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    let path = match parsed.only_positional("build", "source file") {
        Ok(path) => Path::new(path).to_path_buf(),
        Err(error) => {
            eprintln!("{error}\n\n{usage}");
            return ExitCode::FAILURE;
        }
    };
    let (language, source) = match read(&path) {
        Ok(read) => read,
        Err(error) => {
            eprintln!("lamella build: {error}");
            return ExitCode::FAILURE;
        }
    };

    let tier = crate::flash::Tier::from_options(&parsed);
    let libraries: Vec<crate::flash::Library> = Vec::new();

    if let Some(name) = parsed.value("--format") {
        return build_flashable(
            &path,
            &source,
            language,
            name,
            parsed.value("--board"),
            parsed.value("--out"),
            parsed.flag("--unsafe"),
            tier,
            &libraries,
        );
    }

    if tier == crate::flash::Tier::ClassLibrary {
        eprintln!(
            "{}",
            crate::flash::tier_flag_where_nothing_links(
                "build",
                "Without --format this builds the ordinary artifact -- an assembly, a baked image \
                 or a\nPython bundle -- and none of those has a link step.",
                "--format <f> asks for the image a chip takes, which is the build that \
                 links:\n\n\x20   lamella build <file> --board <id> --format bin \
                 --class-library",
                "Nothing was built.",
            )
        );
        return ExitCode::FAILURE;
    }

    if crate::flash::is_project(&path) {
        eprintln!(
            "lamella build: {} is a project, and this build is not producing a chip image.\n\n\
             --format <f> asks for the image a chip takes, which is the artifact a project \
             describes:\n\n\
             \x20   lamella build {} --board <id> --format bin\n\n\
             Nothing was built.",
            path.display(),
            path.display()
        );
        return ExitCode::FAILURE;
    }

    let built = match language {
        Language::CSharp => build_csharp(&path, &source, parsed.flag("--unsafe")),
        Language::Python => build_python(&path, &source, parsed.value("--board")),
        Language::JavaScript => Err(format!(
            "{}: lamella build produces a device artifact, and the JavaScript tier runs on this \
             machine only.
Use lamella run to execute it here.",
            path.display()
        )),
    };
    let built = match built {
        Ok(built) => built,
        Err(error) => {
            eprintln!("lamella build: {error}");
            return ExitCode::FAILURE;
        }
    };

    let out = match parsed.value("--out") {
        Some(given) => Path::new(given).to_path_buf(),
        None => path.with_extension(built.extension),
    };
    if let Err(error) = std::fs::write(&out, &built.bytes) {
        eprintln!("lamella build: write {}: {error}", out.display());
        return ExitCode::FAILURE;
    }
    println!("{} <- {}", out.display(), path.display());
    match built.note {
        Some(note) => println!("  {} ({note})  {} B", built.what, built.bytes.len()),
        None => println!("  {}  {} B", built.what, built.bytes.len()),
    }

    let Some(board_id) = parsed.value("--board") else {
        return ExitCode::SUCCESS;
    };
    answer_fit(board_id, &built)
}

/// The verb's usage text.
///
/// **THE FORMATS ARE READ FROM THE TABLE `--format` IS PARSED AGAINST**, one row each, so this text
/// cannot offer a format the parser refuses or leave out one it takes.
fn usage() -> String {
    let formats: String = lamella_flash_routes::artifact::Output::all()
        .map(|output| match output.gloss() {
            Some(gloss) => format!("\n    {:<5} {gloss}", output.extension()),
            None => format!("\n    {}", output.extension()),
        })
        .collect();
    format!(
        "\
usage: lamella build <file.cs|file.csproj|file.py> [--board <id>] [--format <f>] [--out <path>]
                                      [--class-library]

With --format, it builds the BARE-METAL IMAGE for --board and writes it in that format -- which is
exactly what `lamella flash` takes, so `build` produces what `flash` consumes and neither has to
touch hardware. The formats:
{formats}

Without --format it builds the ordinary artifact -- an assembly, a baked image, or a Python bundle
-- and with --board it also answers whether that fits.

A .csproj builds every .cs beside it as ONE program and links the assemblies its <Reference>
elements name, each by a <HintPath>. It goes with --format, which is the build that links.

--class-library links the program with the class library and the runtime support archive, so it may
allocate, use floating point and call into System.*. That tier's collector reclaims an object
without finalizing it, so a finalizer (a class's ~destructor) never runs there. Without it the flat
tier is used, which is linker-free and resolves no call outside the program. Every build says which
tier produced it. The class-library tier covers fewer boards; asking for it where there is no plan
names the ones there are.

--format elf writes the image as a linked ELF that also carries the program's debug information,
which is the file a debugger takes as the program. It goes with --class-library, the one tier
that carries debug information into an image.

A class-library image is always compiled as a debug build is, whatever format it is written in:
--format elf writes the debug information beside it and every other format sets it aside. So the
image a board is written with is the one --format elf describes, and a debugger can attach to any
board running it. The flat tier is compiled without debug information.
"
    )
}

/// `lamella build <file> --board <id> --format <f>`: the image a chip takes, written to a file and
/// nowhere else.
///
/// **THIS IS THE VERB THAT MAKES AN IMAGE WITHOUT PUTTING IT ANYWHERE**, which is what a release
/// pipeline, a colleague on another machine, and a vendor's own programming tool all need. What it
/// writes is byte-for-byte what `deploy --board` would have written to the chip; the only
/// difference is where it goes.
fn build_flashable(
    path: &Path,
    source: &str,
    language: Language,
    format_name: &str,
    board_id: Option<&str>,
    out: Option<&str>,
    unsafe_code: bool,
    tier: crate::flash::Tier,
    libraries: &[crate::flash::Library],
) -> ExitCode {
    let output = match lamella_flash_routes::artifact::Output::parse(format_name) {
        Ok(output) => output,
        Err(error) => {
            eprintln!("lamella build: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(board_id) = board_id else {
        eprintln!(
            "lamella build: --format wants --board too.\n\n\
             A record format describes an image for a specific chip -- its addresses are that \
             chip's -- so\nthere is no board-independent answer to write."
        );
        return ExitCode::FAILURE;
    };
    if language != Language::CSharp {
        eprintln!(
            "lamella build: --format builds an ahead-of-time C# image today; the Python tier \
             reaches a\nbare-metal image through a separate lowering."
        );
        return ExitCode::FAILURE;
    }
    let format = match output {
        lamella_flash_routes::artifact::Output::Image(format) => format,
        lamella_flash_routes::artifact::Output::Elf => {
            return build_debug_elf(path, source, board_id, out, unsafe_code, tier, libraries);
        }
    };

    let (image, base) =
        match crate::flash::image_for_board(path, source, board_id, unsafe_code, tier, libraries) {
            Ok(built) => built,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        };
    let format = match (format, crate::flash::uf2_family_for_board(board_id)) {
        (lamella_flash_routes::artifact::Format::Uf2 { .. }, None) => {
            eprintln!(
                "lamella build: {board_id} is not written by copying an image to a volume, so a \
                 UF2 for it\nwould name no chip family and every bootloader would refuse it. Try \
                 --format hex or bin."
            );
            return ExitCode::FAILURE;
        }
        (format, Some(family)) => format.for_family(family),
        (format, None) => format,
    };
    let rendered = format.render(&image, base);
    let out = match out {
        Some(given) => Path::new(given).to_path_buf(),
        None => path.with_extension(format.extension()),
    };
    if let Err(error) = std::fs::write(&out, &rendered) {
        eprintln!("lamella build: write {}: {error}", out.display());
        return ExitCode::FAILURE;
    }
    println!("{} <- {}", out.display(), path.display());
    println!(
        "  {} at {base:#010x}, {} B of image in {} B of {}",
        format.description(),
        image.len(),
        rendered.len(),
        format.description()
    );
    println!("  {}", tier.line());
    println!("\nwrite it with:\n    lamella flash {} --board {board_id}", out.display());
    ExitCode::SUCCESS
}

/// `lamella build <file> --board <id> --class-library --format elf`: the image with the program's
/// debug information, as the linked ELF a debugger takes as the program.
///
/// **ITS LOADED BYTES ARE THE IMAGE `deploy --class-library` WRITES**, because every class-library
/// image is compiled as a debug build is: the image formats set the debug information aside, and
/// this writes it beside the image. So the file is as flashable as the image formats -- `lamella
/// flash` reads an ELF -- a debugger shown it is shown the program the board runs, and a debugger
/// can attach to any board a class-library image of the same program was deployed to. The flat
/// tier is compiled without debug information and has no ELF.
fn build_debug_elf(
    path: &Path,
    source: &str,
    board_id: &str,
    out: Option<&str>,
    unsafe_code: bool,
    tier: crate::flash::Tier,
    libraries: &[crate::flash::Library],
) -> ExitCode {
    if let Some(refusal) = elf_refusal(tier) {
        eprintln!("{refusal}");
        return ExitCode::FAILURE;
    }
    let elf =
        match crate::flash::debug_elf_for_board(path, source, board_id, unsafe_code, libraries) {
            Ok(elf) => elf,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        };
    let loaded = match lamella_elf::flat_image(&elf) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("lamella build: the ELF this built does not read back as one: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let out = match out {
        Some(given) => Path::new(given).to_path_buf(),
        None => path.with_extension(lamella_flash_routes::artifact::Output::Elf.extension()),
    };
    if let Err(error) = std::fs::write(&out, &elf) {
        eprintln!("lamella build: write {}: {error}", out.display());
        return ExitCode::FAILURE;
    }
    println!("{} <- {}", out.display(), path.display());
    println!(
        "  ELF at {:#010x}, {} B of image and the program's debug information in {} B of ELF",
        loaded.base,
        loaded.bytes.len(),
        elf.len()
    );
    println!("  {}", tier.line());
    println!("\nwrite it with:\n    lamella flash {} --board {board_id}", out.display());
    ExitCode::SUCCESS
}

/// Why `--format elf` cannot be written on `tier`, or `None` where it can.
///
/// **ONLY THE CLASS-LIBRARY TIER CARRIES DEBUG INFORMATION INTO AN IMAGE**, so on the flat tier the
/// refusal names the flag that makes the ELF work, and the formats the flat tier's image can be
/// written in instead.
fn elf_refusal(tier: crate::flash::Tier) -> Option<String> {
    if tier == crate::flash::Tier::ClassLibrary {
        return None;
    }
    Some(format!(
        "lamella build: --format elf writes the image with the program's debug information, and \
         only the\nclass-library tier carries debug information into an image. Add {}, or write \
         the flat\ntier's image in a format that carries none:\n\n\x20   {}\n\n\
         Nothing was built.",
        crate::flash::CLASS_LIBRARY_FLAG,
        lamella_flash_routes::artifact::Format::listing()
    ))
}

/// The name of each artifact this verb can produce, named once so the rule about those names is
/// asked of the names the code uses rather than of a copy of them.
const BAKED_FLASH_IMAGE: &str = "baked flash image";
const ASSEMBLY: &str = "assembly";
const PYTHON_BUNDLE: &str = "Python bundle";

/// What a build produced.
struct Built {
    /// The artifact.
    bytes: Vec<u8>,
    /// The extension it is conventionally written with.
    extension: &'static str,
    /// What the artifact IS, in the terms the rest of the toolchain uses for it.
    ///
    /// **A NOUN PHRASE, BECAUSE TWO OF ITS THREE READERS PUT IT IN THE MIDDLE OF A SENTENCE.**
    /// It follows "cannot run this" and "the number compared is the", so anything that is not a
    /// plain name for the thing arrives inside prose that then reads as nonsense. A warning about
    /// the artifact belongs in [`Built::note`] or [`Built::excludes`]; this is only its name.
    what: &'static str,
    /// A warning about the artifact, shown beside its name where it is REPORTED and nowhere else.
    ///
    /// Separate from [`Built::what`] because a name and a warning are read in different places:
    /// this is printed in parentheses after the artifact line, where a reader is looking at what
    /// they just got, and it is never spliced into a sentence about the board.
    note: Option<&'static str>,
    /// **WHAT THE BYTE COUNT DOES NOT INCLUDE, AND THEREFORE WHAT A FIT VERDICT OVER IT MEANS.**
    ///
    /// A fit verdict compares a number against a board's whole flash budget, which is the right
    /// comparison only when the artifact is the whole flash occupant. For a tier where the board
    /// is already running firmware that the image is loaded INTO, it is not -- the headroom is an
    /// upper bound rather than the space the image will have. Carried with the artifact rather
    /// than reconstructed at the comparison, so the caveat cannot be attached to the wrong tier.
    excludes: &'static str,
    /// **WHETHER THIS ARTIFACT IS LOADED INTO FIRMWARE ALREADY ON THE BOARD**, rather than being the
    /// whole flash occupant.
    ///
    /// A fit verdict over an artifact of this kind presumes a board that can HOLD that firmware, and
    /// a board declaring no carrier never can. Declared by the artifact for the same reason
    /// [`Built::excludes`] is: reconstructing it at the comparison is how an answer gets attached to
    /// the wrong tier, and an artifact kind added later has to state its own rather than inherit
    /// whichever happened to be true when the check was written.
    loaded_into_firmware: bool,
}

/// Compile a C# program to a .NET assembly.
///
/// **THE ASSEMBLY IS NAMED AFTER THE FILE, WHICH IS WHY THIS DOES NOT GO THROUGH THE REPL COMPILE
/// SEAM.** `LcscCompiler::compile` names every assembly `__Repl` and gives its debug info the
/// source path `Repl.cs`, which is exactly right for a submission typed at a prompt and wrong for
/// an artifact written to disk: the name is what another assembly REFERENCES, and the source path
/// is where a debugger looks. `run` can use the seam because neither is observable there.
///
/// A `--features bake` build turns the assembly into the flash image a device runs; without it the
/// assembly is as far as this verb goes. See the crate documentation for why that feature is not
/// on by default.
fn build_csharp(path: &Path, source: &str, unsafe_code: bool) -> Result<Built, String> {
    let assembly = compile_csharp_assembly(path, source, unsafe_code, "build")?;
    #[cfg(feature = "bake")]
    {
        let image = crate::bake::bake(assembly)?;
        return Ok(Built {
            bytes: image,
            extension: "lmli",
            what: BAKED_FLASH_IMAGE,
            note: None,
            excludes: "the serve firmware already resident on the board, which this image is \
                       loaded INTO rather than replacing",
            loaded_into_firmware: true,
        });
    }
    #[cfg(not(feature = "bake"))]
    Ok(Built {
        bytes: assembly,
        extension: "dll",
        what: ASSEMBLY,
        note: Some("NOT a flash image -- this build has no `bake` feature"),
        excludes: "everything the device supplies -- this is the assembly, not an image. \
                   Build the tool with `--features bake` for the flash image a board runs",
        loaded_into_firmware: true,
    })
}

/// Compile a C# file to a .NET assembly named after it.
///
/// The one place a source FILE becomes an assembly, so `build` and `flash` cannot disagree about
/// what compiling one means -- which reference assemblies it binds against, what the assembly is
/// called, and what its debug info says the source path is.
///
/// # Errors
/// The compiler's diagnostics, rendered as `CSnnnn` lines, or the emit error when binding was
/// clean and a construct is not lowered.
pub fn compile_csharp_assembly(
    path: &Path,
    source: &str,
    unsafe_code: bool,
    verb: &str,
) -> Result<Vec<u8>, String> {
    compile_csharp_assembly_with_corlib(path, source, unsafe_code, &[], verb)
        .map(|(assembly, _)| assembly)
}

/// As [`compile_csharp_assembly`], and also the corlib the program was BOUND against.
///
/// **THE LINKED TIER LINKS THE SAME CORLIB THE PROGRAM WAS BOUND AGAINST, AND THAT IS WHY IT COMES
/// BACK FROM HERE RATHER THAN FROM A SECOND LOOKUP.** Discovery walks an environment variable, then
/// beside the executable, then the development tree, and a second walk can answer differently from
/// the first -- a variable set between them, a file appearing beside the binary. A program bound
/// against one corlib and linked against another would produce an image whose method tokens resolve
/// to the wrong members, which is a wrong answer at run time rather than a link error.
///
/// `libraries` are the class libraries named on the command line, **in the order given**: the
/// program BINDS against them here and LINKS against them later, and it has to be the same set in
/// the same order or the build would resolve a name at compile time that the link cannot find.
///
/// # Errors
/// As [`compile_csharp_assembly`], plus a reference set carrying no corlib to hand back, or a named
/// library that is not a readable assembly.
pub fn compile_csharp_assembly_with_corlib(
    path: &Path,
    source: &str,
    unsafe_code: bool,
    libraries: &[crate::flash::Library],
    verb: &str,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    compile_csharp_file(path, source, unsafe_code, libraries, verb, false)
        .map(|compiled| (compiled.assembly, compiled.corlib))
}

/// As [`compile_csharp_assembly_with_corlib`], compiled for a debugger: the program, the corlib it
/// was bound against, and the Portable PDB that maps its IL back to its source.
///
/// **THE PDB NAMES THE SOURCE BY ITS ABSOLUTE PATH** ([`document_path`]), because that is the path
/// an editor sends with a breakpoint. The compiler's diagnostics still name the file as it was
/// typed.
///
/// # Errors
/// As [`compile_csharp_assembly_with_corlib`], plus a path that cannot be made absolute.
pub fn compile_csharp_for_debugging(
    path: &Path,
    source: &str,
    unsafe_code: bool,
    libraries: &[crate::flash::Library],
    verb: &str,
) -> Result<Debuggable, String> {
    let compiled = compile_csharp_file(path, source, unsafe_code, libraries, verb, true)?;
    compiled.debuggable(|| format!("lamella {verb}: {}", path.display()))
}

/// A C# program compiled for a debugger, as [`compile_csharp_for_debugging`] and
/// [`compile_project_for_debugging`] produce it.
pub struct Debuggable {
    /// The assembly.
    pub assembly: Vec<u8>,
    /// The corlib the program was bound against, which is the one its link has to take.
    pub corlib: Vec<u8>,
    /// The standalone Portable PDB: each method's sequence points and local names, and each
    /// source by its absolute path.
    pub pdb: Vec<u8>,
}

/// What one compilation produced: the assembly, its corlib, and the PDB when one was asked for.
struct Compiled {
    assembly: Vec<u8>,
    corlib: Vec<u8>,
    pdb: Option<Vec<u8>>,
}

impl Compiled {
    /// This compilation as a [`Debuggable`], or a refusal led by `subject` when it carries no PDB.
    fn debuggable(self, subject: impl FnOnce() -> String) -> Result<Debuggable, String> {
        let Compiled { assembly, corlib, pdb } = self;
        match pdb {
            Some(pdb) => Ok(Debuggable { assembly, corlib, pdb }),
            None => Err(format!(
                "{}: the compiler was asked for debug information and wrote none.",
                subject()
            )),
        }
    }
}

/// The path a debugger is given for a source file: absolute, and with no `.` or `..` in it.
///
/// **AN EDITOR SENDS A BREAKPOINT WITH THE FILE'S ABSOLUTE PATH, AND THE DEBUG INFORMATION HAS TO
/// NAME THE SAME ONE.** A relative path names the directory the build happened to run in, which
/// no editor sends. The `.` and `..` components are resolved from the path alone, not from the
/// disk, so a symbolic link stays where the developer wrote it.
fn document_path(path: &Path) -> std::io::Result<String> {
    let absolute = std::path::absolute(path)?;
    Ok(without_dot_segments(&absolute).display().to_string())
}

/// `path` with its `.` and `..` components resolved from the path alone: a `..` removes the
/// component before it, and at the root it stays at the root.
fn without_dot_segments(path: &Path) -> std::path::PathBuf {
    let mut resolved = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

/// The one single-file compilation behind [`compile_csharp_assembly_with_corlib`] and
/// [`compile_csharp_for_debugging`]; `debug` also asks the compiler for the PDB.
///
/// **ONE PATH, SO A PROGRAM IS BOUND THE SAME WAY WHETHER IT IS DEPLOYED OR DEBUGGED**: against
/// the same references in the same order, under the same assembly name and the same options.
fn compile_csharp_file(
    path: &Path,
    source: &str,
    unsafe_code: bool,
    libraries: &[crate::flash::Library],
    verb: &str,
    debug: bool,
) -> Result<Compiled, String> {
    let compiler = LcscCompiler::discover()?;
    let mut references: Vec<lamella_metadata::Assembly> = compiler
        .references()
        .iter()
        .filter_map(|bytes| lamella_metadata::Assembly::read(bytes).ok())
        .collect();
    for library in libraries {
        let parsed = lamella_metadata::Assembly::read(library.bytes()).map_err(|error| {
            format!(
                "lamella {verb}: {} is not a readable .NET assembly: {error:?}\n\n\
                 A <Reference> wants a path to a built `.dll`. `lcsc /target:library` \
                 produces one.",
                library.path().display()
            )
        })?;
        references.push(parsed);
    }
    let name = assembly_name(path);
    let options = lamella_syntax::lexer::LexOptions {
        unsafe_code,
        file_based: true,
        ..Default::default()
    };
    refuse_unhonored_directives(source, options.clone())?;
    let typed = path.display().to_string();
    let document = if debug {
        document_path(path).map_err(|error| {
            format!(
                "lamella: {typed}: cannot make it an absolute path for its debug information: \
                 {error}"
            )
        })?
    } else {
        typed.clone()
    };
    let compiled = lamella_assemble::compile_source_with(
        source,
        &document,
        &name,
        &name,
        &references,
        debug,
        options,
    );
    let Some(image) = compiled.image else {
        return Err(render_diagnostics(&compiled, &typed, source));
    };
    let Some(corlib) = compiler.references().first().cloned() else {
        return Err("the compiler found no reference assemblies".to_owned());
    };
    Ok(Compiled {
        assembly: image,
        corlib,
        pdb: compiled.pdb,
    })
}

/// A metadata assembly name derived from `path`.
///
/// A path may hold spaces, dots and separators; an assembly name is an identifier that other
/// assemblies write down. Anything outside the identifier set folds to `_`, and an empty or
/// digit-leading stem gains a prefix, so every file produces a name something can reference.
fn assembly_name(path: &Path) -> String {
    let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("program");
    let mut name: String = stem
        .chars()
        .map(|ch| if ch.is_alphanumeric() || ch == '_' { ch } else { '_' })
        .collect();
    if name.is_empty() || name.starts_with(|ch: char| ch.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name
}

/// Compile every source a project names into ONE assembly, and the corlib it was bound against.
///
/// **THE PROJECT'S FILES ARE ONE COMPILATION, NOT SEVERAL.** Each file's types enter one model
/// before any body binds, so a type declared in one names a type declared in another -- which is
/// what a multi-file compilation means and what somebody splitting a program across files expects.
/// Compiling them separately and linking after would make declaration order across files matter.
///
///
/// # Errors
/// A source that cannot be read, the compiler's diagnostics, or a declared reference that is not a
/// readable assembly.
pub fn compile_project_assembly(
    project: &crate::project::Project,
    libraries: &[crate::flash::Library],
    verb: &str,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    compile_project(project, libraries, verb, false)
        .map(|compiled| (compiled.assembly, compiled.corlib))
}

/// As [`compile_project_assembly`], compiled for a debugger: the program, its corlib, and the
/// Portable PDB, which names each of the project's sources by its absolute path
/// ([`document_path`]).
///
/// # Errors
/// As [`compile_project_assembly`], plus a source path that cannot be made absolute.
pub fn compile_project_for_debugging(
    project: &crate::project::Project,
    libraries: &[crate::flash::Library],
    verb: &str,
) -> Result<Debuggable, String> {
    let compiled = compile_project(project, libraries, verb, true)?;
    compiled.debuggable(|| format!("lamella {verb}: {}", project.assembly_name))
}

/// The one project compilation behind [`compile_project_assembly`] and
/// [`compile_project_for_debugging`]; `debug` also asks the compiler for the PDB.
fn compile_project(
    project: &crate::project::Project,
    libraries: &[crate::flash::Library],
    verb: &str,
    debug: bool,
) -> Result<Compiled, String> {
    let compiler = LcscCompiler::discover()?;
    let mut references: Vec<lamella_metadata::Assembly> = compiler
        .references()
        .iter()
        .filter_map(|bytes| lamella_metadata::Assembly::read(bytes).ok())
        .collect();
    for library in libraries {
        let parsed = lamella_metadata::Assembly::read(library.bytes()).map_err(|error| {
            format!(
                "lamella {verb}: {} is not a readable .NET assembly: {error:?}\n\n\
                 A <Reference> wants a path to a built `.dll`. `lcsc /target:library` \
                 produces one.",
                library.path().display()
            )
        })?;
        references.push(parsed);
    }
    let mut texts = Vec::with_capacity(project.sources.len());
    let mut documents = Vec::with_capacity(project.sources.len());
    for source in &project.sources {
        let text = std::fs::read_to_string(source).map_err(|error| {
            format!(
                "lamella {verb}: read {}: {error}\n\n\
                 The project names it, so the build stops rather than compiling a program \
                 that is missing\na file.",
                source.display()
            )
        })?;
        let typed = source.display().to_string();
        documents.push(if debug {
            document_path(source).map_err(|error| {
                format!(
                    "lamella {verb}: {typed}: cannot make it an absolute path for its debug \
                     information: {error}"
                )
            })?
        } else {
            typed.clone()
        });
        texts.push((text, typed));
    }
    let sources: Vec<(&str, &str)> = texts
        .iter()
        .zip(&documents)
        .map(|((text, _), document)| (text.as_str(), document.as_str()))
        .collect();
    let options = lamella_syntax::lexer::LexOptions {
        unsafe_code: project.allow_unsafe,
        file_based: false,
        ..Default::default()
    };
    let compiled = lamella_assemble::compile_sources_with(
        &sources,
        &project.assembly_name,
        &project.assembly_name,
        &references,
        debug,
        options,
    );
    let Some(image) = compiled.image else {
        return Err(render_multi_diagnostics(&compiled, &texts));
    };
    let Some(corlib) = compiler.references().first().cloned() else {
        return Err("the compiler found no reference assemblies".to_owned());
    };
    Ok(Compiled {
        assembly: image,
        corlib,
        pdb: compiled.pdb,
    })
}

/// A multi-file compilation's diagnostics, each attributed to the file it came from.
///
/// **THE FILE NAME IS THE POINT.** A project compiles several sources into one assembly, so a bare
/// `CS0246` with no path leaves the reader searching every file they listed -- and the diagnostic
/// lists arrive parallel to the input order precisely so that does not have to happen. The line
/// and column come with it, out of the text of the file the diagnostic is attributed to.
fn render_multi_diagnostics(
    compiled: &lamella_assemble::MultiCompilation,
    sources: &[(String, String)],
) -> String {
    if let Some(emit_error) = &compiled.emit_error {
        return format!("error: this construct is not yet supported by lcsc: {emit_error}");
    }
    let mut text = String::new();
    for (per_file, (file_text, path)) in compiled.diagnostics.iter().zip(sources) {
        for diagnostic in per_file {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&diagnostic.render(path, file_text));
        }
    }
    if text.is_empty() {
        text.push_str("compilation produced no image");
    }
    if text.contains("CS0227") {
        text.push_str(
            "\n\nunsafe code is off by default, as it is in csc without /unsafe. Turn it on in the \
             project:\n    <AllowUnsafeBlocks>true</AllowUnsafeBlocks>",
        );
    }
    text
}

/// A single file's diagnostics, each naming the file and the line it came from -- or, when binding
/// was clean and a construct is not lowered, the emit error.
///
/// `path` and `source` are the ones that were compiled: the location is read out of the text the
/// diagnostic's span indexes, so a caller that passed a different file would report a real code
/// at an imaginary line.
fn render_diagnostics(
    compiled: &lamella_assemble::Compilation,
    path: &str,
    source: &str,
) -> String {
    if let Some(emit_error) = &compiled.emit_error {
        return format!("{path}: error: this construct is not yet supported by lcsc: {emit_error}");
    }
    let mut text = String::new();
    for diagnostic in &compiled.diagnostics {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&diagnostic.render(path, source));
    }
    if text.is_empty() {
        text.push_str("compilation produced no image");
    }
    if text.contains("CS0227") {
        text.push_str(
            "\n\nunsafe code is off by default, as it is in csc without /unsafe. Pass --unsafe to \
             allow it:\n    lamella build <file> --unsafe",
        );
    }
    text
}

/// Compile a Python program to the bundle a device runs.
fn build_python(path: &Path, source: &str, board: Option<&str>) -> Result<Built, String> {
    let bundle = compile_python(path, source, board)?;
    let modules = bundle.modules.len();
    let bytes = bundle.encode(lamella_py_bytecode::FeatureFlags::FIRST_LIGHT);
    match lamella_py_bytecode::Bundle::decode(&bytes) {
        Ok((round_tripped, _)) if round_tripped.modules.len() == modules => {}
        Ok((round_tripped, _)) => {
            return Err(format!(
                "the bundle encoded {modules} module(s) and decoded {}",
                round_tripped.modules.len()
            ));
        }
        Err(error) => return Err(format!("the bundle does not decode: {error:?}")),
    }
    Ok(Built {
        bytes,
        extension: "lpyc",
        what: PYTHON_BUNDLE,
        note: None,
        excludes: "the Python interpreter firmware, which is by far the larger half and is \
                   already on the board",
        loaded_into_firmware: true,
    })
}

/// The exit code the interpreter uses when a program is stopped by an exception nothing caught.
const ABORTED_ON_EXCEPTION: i32 = 70;

/// What a nonzero exit means, where this tool knows something the number does not say.
///
/// **70 IS TWO DIFFERENT ANSWERS AND THE RUNNER CANNOT TELL THEM APART.** The interpreter aborts on
/// an unhandled exception with 70, and a program whose own `Main` returns 70 exits identically --
/// so `lamella run` printed the same line for a thrown `InvalidOperationException` and for
/// `return 70`, with nothing to choose between them.
///
/// **THE EXCEPTION'S TYPE, MESSAGE AND LOCATION DO NOT CROSS THE SEAM AT ALL**, so this cannot
/// report them and must not imply that looking harder would find them. What it can do is say which
/// two things the number means and name the way a program can usually answer it itself -- its own
/// output DOES cross, so a `catch` that prints reaches the reader when the abort does not.
///
/// **"USUALLY" IS MEASURED AND NOT A HEDGE.** Some aborts skip the handler: a `new GpioController()`
/// on the host exits 70 with neither the `try` body's output nor the `catch`'s, which is a trap
/// rather than an exception and no handler can see it. Promising that a `catch` always answers the
/// question would send those readers to write one that stays silent.
///
/// Empty for every other code: a program that returns 3 means whatever its author decided, and this
/// tool has nothing to add to it.
fn exit_note(exit: i32) -> String {
    if exit != ABORTED_ON_EXCEPTION {
        return String::new();
    }
    "\nThat code means one of two things, and the code alone cannot tell you which:\n\
     \x20 - the program was stopped by an exception nothing caught, or\n\
     \x20 - its own Main returned 70.\n\n\
     A TRAP: line above this one settles it. It is printed when an exception escaped, and it\n\
     carries that exception's type and message. No TRAP: line means the program returned 70 itself.\n\n\
     Some aborts skip the handler and print nothing further -- which is itself the answer, because\n\
     an exception a catch cannot see is not an exception.\n"
        .to_owned()
}

/// Why no fit verdict can be given for `built` on `board`, when that is the case.
///
/// **A BOARD THAT DECLARES NO CARRIER CANNOT RUN AN ARTIFACT THAT IS LOADED INTO FIRMWARE**, so the
/// arithmetic is sound and the question is the wrong one. A carrier records how a Lamella Link wire
/// reaches a board; a part with no room for an interpreter and a wire protocol declares none, and
/// then there is no resident firmware for an image to be loaded into, and never will be.
///
/// **THE HEADROOM IS NOT MERELY UNHELPFUL THERE -- THE CAVEAT ABOVE IT IS FALSE.** That caveat says
/// the figure excludes "the serve firmware already resident on the board", which asserts a firmware
/// this board cannot hold: a reader is told the number is an upper bound because of something that
/// does not exist.
///
/// Answered here rather than inside the fit rule because the rule is given a byte count and this
/// needs the artifact's TIER. `lamella fit --board <id> --image-bytes <n>` and the editor's fit tool
/// are handed a bare number and structurally cannot know it; this path holds the artifact.
fn no_carrier_refusal(
    board_id: &str,
    board: &lamella_bsp_gen::strata::BoardTable,
    built: &Built,
) -> Option<String> {
    if !built.loaded_into_firmware || !board.carrier.kind.is_empty() {
        return None;
    }
    Some(format!(
        "lamella build: {board_id} cannot run this {}.\n\n\
         It declares no carrier -- no Lamella Link wire reaches it -- so there is no resident \
         firmware for\nthis artifact to be loaded into, and a flash headroom figure would measure \
         it against a tier the\nboard does not have.\n\n\
         That is a property of the part rather than a gap in this build. The artifact itself was \
         written\nand is unaffected; it is the fit question that has no answer here.\n\n\
         `lamella boards` lists what each board can be given.",
        built.what
    ))
}

/// Answer "does this fit on that board" about what was just built.
///
/// **THIS IS THE HALF `fit` CANNOT REACH ON ITS OWN.** `lamella fit` wants an image size, and this
/// is the verb that produces one; without it the question can be asked only by somebody who
/// already knows the answer.
fn answer_fit(board_id: &str, built: &Built) -> ExitCode {
    let (board, part) = match catalog::resolve(board_id) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("lamella build: {error}");
            return ExitCode::FAILURE;
        }
    };
    let image_bytes = match i64::try_from(built.bytes.len()) {
        Ok(bytes) => bytes,
        Err(_) => {
            eprintln!("lamella build: the artifact is too large to compare against a budget");
            return ExitCode::FAILURE;
        }
    };
    if let Some(refusal) = no_carrier_refusal(board_id, &board, built) {
        eprintln!("{refusal}");
        return ExitCode::FAILURE;
    }
    println!("\ndoes it fit on {board_id}?");
    println!("  the number compared is the {}, which excludes", built.what);
    println!("  {},", built.excludes);
    println!("  so the headroom below is an UPPER BOUND rather than the room this image has.\n");
    let verdict = fit(&board, &part, image_bytes);
    print!("{}", crate::verdicts::render(board_id, &verdict));
    crate::verdicts::exit_for(&verdict)
}

/// Read a source file and decide its language.
fn read(path: &Path) -> Result<(Language, String), String> {
    if crate::flash::is_project(path) {
        return Ok((Language::CSharp, String::new()));
    }
    let language = Language::of(path)?;
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok((language, source))
}

/// Nanoseconds since the Unix epoch. Saturates rather than panicking on a system clock set before
/// 1970, which is a broken machine rather than a program error.
fn host_wall_ns() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_nanos()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// Nanoseconds from a fixed origin in this process, which is all a monotonic clock promises.
fn host_monotonic_ns() -> i64 {
    use std::sync::OnceLock;
    static ORIGIN: OnceLock<std::time::Instant> = OnceLock::new();
    let origin = ORIGIN.get_or_init(std::time::Instant::now);
    i64::try_from(origin.elapsed().as_nanos()).unwrap_or(i64::MAX)
}

fn host_sleep_ns(nanos: i64) {
    std::thread::sleep(std::time::Duration::from_nanos(nanos.max(0).unsigned_abs()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project directory of the test's own: `App.csproj` of `output_type` with `items` inside its
    /// `<Project>` element, and `Program.cs` beside it holding `program`.
    fn project_at(name: &str, output_type: &str, items: &str, program: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lamella-cli-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        std::fs::write(
            dir.join("App.csproj"),
            format!(
                "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    \
                 <OutputType>{output_type}</OutputType>\n    \
                 <TargetFramework>lamella1.0</TargetFramework>\n  </PropertyGroup>\n{items}</Project>\n"
            ),
        )
        .expect("write the project");
        std::fs::write(dir.join("Program.cs"), program).expect("write the program");
        dir.join("App.csproj")
    }

    const PRINTS: &str = "class Program\n{\n    static void Main()\n    {\n        \
                          System.Console.WriteLine(\"from the project\");\n    }\n}\n";

    /// **A PROJECT RUNS AS `build` COMPILES IT.** `run` compiled the project file's own text, which
    /// holds no source, and reported a project with a perfectly good `Main` as having no entry point.
    #[test]
    fn a_project_runs_on_this_machine_as_build_compiles_it() {
        if LcscCompiler::discover().is_err() {
            return;
        }
        let project = project_at("runs", "Exe", "", PRINTS);
        let exit = run_command(&[project.to_string_lossy().into_owned()]);
        assert_eq!(format!("{exit:?}"), format!("{:?}", ExitCode::SUCCESS));
        match project_outcome(&project) {
            Ok(Ok(Outcome::Ran { output, exit, .. })) => {
                assert_eq!(output, "from the project\n", "the program's own output");
                assert_eq!(exit, 0);
            }
            Ok(Err(error)) => panic!("the run failed: {error:?}"),
            Ok(Ok(_)) => panic!("the project ran as something other than a program"),
            Err(refusal) => panic!("refused: {refusal}"),
        }
        let _ = std::fs::remove_dir_all(project.parent().expect("its directory"));
    }

    /// A class library has nothing to run, and a project that names libraries of its own would run
    /// here without them, so both are refused -- each saying what does work, in prose that renders.
    #[test]
    fn a_library_project_and_one_naming_libraries_are_refused_by_run() {
        let library = project_at("library", "Library", "", PRINTS);
        let refusal = project_outcome(&library).expect_err("a class library has no entry point");
        assert!(
            refusal.contains("builds a class library") && refusal.contains("<Reference>"),
            "{refusal}"
        );
        crate::rendered::assert_renders_cleanly(&refusal, crate::rendered::four_space_sample);

        let gpio = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../lamella-load/tests/fixtures/System.Device.Gpio.dll");
        let items = format!(
            "  <ItemGroup>\n    <Reference Include=\"System.Device.Gpio\">\n      \
             <HintPath>{}</HintPath>\n    </Reference>\n  </ItemGroup>\n",
            gpio.display()
        );
        let referencing = project_at("referencing", "Exe", &items, PRINTS);
        let refusal = project_outcome(&referencing).expect_err("its libraries are not linked here");
        assert!(
            refusal.contains("references libraries of its own")
                && refusal.contains("System.Device.Gpio.dll")
                && refusal.contains("--class-library"),
            "{refusal}"
        );
        crate::rendered::assert_renders_cleanly(&refusal, crate::rendered::four_space_sample);
        for project in [library, referencing] {
            let _ = std::fs::remove_dir_all(project.parent().expect("its directory"));
        }
    }

    /// Builds a `Compilation` carrying diagnostics and no image, which is the only state the two
    /// renderers below are reached in.
    fn failed(diagnostics: Vec<lamella_assemble::Diagnostic>) -> lamella_assemble::Compilation {
        lamella_assemble::Compilation {
            diagnostics,
            image: None,
            pdb: None,
            emit_error: None,
        }
    }

    /// A diagnostic in Lamella's own namespace at `offset` in some source.
    fn lamella_only(offset: u32) -> lamella_assemble::Diagnostic {
        lamella_assemble::Diagnostic {
            code: 1,
            namespace: lamella_syntax::diagnostic::CodeNamespace::Lam,
            severity: lamella_syntax::diagnostic::Severity::Error,
            message: String::from("this build cannot emit that construct"),
            span: lamella_syntax::span::Span::new(offset, offset + 1),
        }
    }

    /// What a single-file build PRINTS, pinned: the file, the line, the column, and Lamella's own
    /// prefix on a condition csc has no concept of.
    ///
    /// **THE EXACT TEXT IS THE ASSERTION.** Either renderer can be changed in any direction --
    /// prefix, location, severity word, ordering -- without another test in this crate noticing,
    /// so nothing short of the whole line pins what a reader is shown.
    #[test]
    fn a_single_file_build_names_the_file_the_line_and_the_namespace_it_is_reporting_in() {
        let source = "class C\n{\n    void M() { }\n}\n";
        let offset = source.find("void").expect("the fixture contains `void`") as u32;
        let rendered = render_diagnostics(&failed(vec![lamella_only(offset)]), "App.cs", source);
        assert_eq!(
            rendered,
            "App.cs(3,5): error LAM0001: this build cannot emit that construct"
        );
    }

    /// Each file's diagnostics are rendered against THAT file's text.
    ///
    /// **THE PAIRING IS THE DEFECT THIS CATCHES.** The diagnostic lists arrive parallel to the
    /// input order, so a renderer that zipped them against the wrong source would still print a
    /// real code and a real path -- and a line number taken from a different file, which is the
    /// kind of wrong answer a reader believes. The two fixtures put the same offset on different
    /// lines precisely so a crossed pairing cannot produce the expected text.
    #[test]
    fn a_project_build_renders_each_diagnostic_against_the_file_it_is_attributed_to() {
        let first = String::from("class First { void M() { } }");
        let second = String::from("class\nSecond\n{ void M() { } }");
        let texts = vec![
            (first, String::from("First.cs")),
            (second, String::from("Second.cs")),
        ];
        let compiled = lamella_assemble::MultiCompilation {
            diagnostics: vec![vec![lamella_only(10)], vec![lamella_only(10)]],
            image: None,
            pdb: None,
            emit_error: None,
        };
        let rendered = render_multi_diagnostics(&compiled, &texts);
        assert_eq!(
            rendered,
            concat!(
                "First.cs(1,11): error LAM0001: this build cannot emit that construct\n",
                "Second.cs(2,5): error LAM0001: this build cannot emit that construct"
            )
        );
    }

    /// **THE SAME LINE FOR A THROWN EXCEPTION AND FOR `return 70`.** Measured before this note
    /// existed: a program throwing `InvalidOperationException` and one whose `Main` returns 70
    /// produced byte-identical output, `lamella run: the program exited 70`, with nothing to choose
    /// between them and nothing to read.
    #[test]
    fn exit_seventy_says_which_two_things_it_means_and_does_not_promise_more() {
        let note = exit_note(70);
        assert!(
            note.contains("one of two things"),
            "it names the ambiguity: {note}"
        );
        assert!(
            note.contains("nothing caught"),
            "and the abort case: {note}"
        );
        assert!(
            note.contains("TRAP:"),
            "and it points at the line that settles it: {note}"
        );
        assert!(
            note.contains("returned 70"),
            "and the ordinary case: {note}"
        );
        assert!(
            !note.contains("do not reach here"),
            "and it no longer denies a report the runner now prints: {note}"
        );
        assert!(
            note.contains("catch"),
            "and the way to answer it from the program: {note}"
        );
        assert!(
            note.contains("skip the handler"),
            "and says when it will not fire: {note}"
        );
    }

    /// **EVERY OTHER CODE MEANS WHATEVER ITS AUTHOR DECIDED.** A note attached to those would be
    /// this tool inventing a meaning for somebody else's number.
    #[test]
    fn a_program_that_chose_its_own_exit_code_is_not_annotated() {
        for code in [1, 2, 3, 69, 71, -1] {
            assert!(
                exit_note(code).is_empty(),
                "{code} is the program's own answer"
            );
        }
    }

    fn artifact(what: &'static str, loaded_into_firmware: bool) -> Built {
        Built {
            bytes: vec![0; 1902],
            extension: "lmli",
            what,
            note: None,
            excludes: "the serve firmware already resident on the board",
            loaded_into_firmware,
        }
    }

    /// **THE DEFECT: A BOARD THAT CAN NEVER RUN THE ARTIFACT ANSWERED `FITS`.** Measured before the
    /// fix on this board: `FITS -- 14482 B of flash to spare`, exit 0, for a part whose own fact
    /// file says in published words that it "does not host an interpreter and a wire protocol".
    /// The arithmetic was right and the question was the wrong one.
    #[test]
    fn a_board_with_no_carrier_cannot_be_asked_whether_a_loaded_artifact_fits() {
        let Ok((board, _)) = catalog::resolve("muselab-nano-ch32v003") else {
            return;
        };
        assert!(
            board.carrier.kind.is_empty(),
            "this board is the no-carrier case; the fixture moved"
        );
        let refusal = no_carrier_refusal(
            "muselab-nano-ch32v003",
            &board,
            &artifact("baked flash image", true),
        )
        .expect("a board with no carrier cannot hold a loaded artifact");
        assert!(
            refusal.contains("muselab-nano-ch32v003"),
            "it names the board: {refusal}"
        );
        assert!(
            refusal.contains("baked flash image"),
            "and what was built: {refusal}"
        );
        assert!(
            refusal.contains("no carrier"),
            "and the fact it read: {refusal}"
        );
        assert!(
            refusal.contains("was written"),
            "and that the artifact survived: {refusal}"
        );
        assert!(
            refusal.starts_with(
                "lamella build: muselab-nano-ch32v003 cannot run this baked flash image."
            ),
            "the first line is a sentence: {refusal}"
        );
    }

    /// An artifact's NAME is a name, and its warning is carried apart from it.
    ///
    /// **THE NAME IS READ IN THREE PLACES AND TWO OF THEM ARE MID-SENTENCE**, following "cannot run
    /// this" and "the number compared is the". A name carrying a parenthetical warning is fine on
    /// the artifact line, where a reader is looking at what they just got, and arrives as nonsense
    /// in either sentence -- so the two are separate fields and this asks that they stay separate.
    ///
    /// Every artifact this verb can produce is asked, rather than the one that happened to be
    /// wrong: the defect was introduced by the build with no `bake` feature, which is the arm a
    /// developer on a workstation gets and the one least likely to be read in a refusal.
    #[test]
    fn an_artifacts_name_is_a_name_and_carries_no_warning_of_its_own() {
        for name in [BAKED_FLASH_IMAGE, ASSEMBLY, PYTHON_BUNDLE] {
            assert!(
                !name.contains('(') && !name.contains("NOT"),
                "{name:?} is a name with a warning in it, and two readers put it mid-sentence"
            );
        }
    }

    /// **A BOARD THAT DECLARES A CARRIER IS STILL ANSWERED.** The control, without which this rule
    /// could refuse everything and every assertion above would still pass.
    #[test]
    fn a_board_with_a_carrier_is_still_given_a_verdict() {
        let Ok((board, _)) = catalog::resolve("bbc-micro-bit-v2") else {
            return;
        };
        assert!(
            !board.carrier.kind.is_empty(),
            "this board is the carrier case; the fixture moved"
        );
        assert!(
            no_carrier_refusal(
                "bbc-micro-bit-v2",
                &board,
                &artifact("baked flash image", true)
            )
            .is_none(),
            "a board with a wire can hold resident firmware, so the question is a real one"
        );
    }

    /// An artifact that is NOT loaded into firmware is the whole flash occupant, so a board with no
    /// wire is exactly where it belongs -- that is how this part is reached.
    #[test]
    fn an_artifact_that_is_not_loaded_into_firmware_is_not_refused_for_a_missing_wire() {
        let Ok((board, _)) = catalog::resolve("muselab-nano-ch32v003") else {
            return;
        };
        assert!(
            no_carrier_refusal(
                "muselab-nano-ch32v003",
                &board,
                &artifact("bare-metal image", false)
            )
            .is_none(),
            "an image flashed whole needs no resident firmware and so needs no carrier"
        );
    }

    thread_local! {
        static PRINTED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// A JavaScript program runs, and reaches the host's output seam.
    ///
    /// **THE OUTPUT SEAM IS THE POINT.** ECMA-262 defines no `print` and no `console`, so a verb
    /// that wires the language and stops there runs programs that cannot say anything -- and does
    /// it without erroring, which reads as a broken tool rather than a missing seam. This asserts
    /// the seam is installed by calling it and reading back what arrived.
    #[test]
    fn a_javascript_program_runs_and_can_print() {
        let mut interpreter = Interpreter::new();
        interpreter.define_host_function("print", 1, |interpreter, _this, arguments| {
            let text = arguments.first().map_or_else(String::new, |v| interpreter.describe(v));
            PRINTED.with(|printed| printed.borrow_mut().push(text));
            Completion::Normal(JsValue::Undefined)
        });
        interpreter.set_host_clock(Some(epoch_millis()), monotonic_millis);

        let outcome = interpreter
            .run_source("let t = 0; for (let i = 1; i <= 5; i++) { t += i * i; } print(t);");
        assert!(matches!(outcome, Ok(Completion::Normal(_))), "{outcome:?}");
        PRINTED.with(|printed| {
            assert_eq!(printed.borrow().as_slice(), ["55"], "the host function was reached");
        });
    }

    /// The elapsed-time source is monotonic, which the wall clock is not.
    ///
    /// `Date.now()` anchors on the wall clock and ADDS elapsed monotonic time. Feeding the wall
    /// clock to both lets a backwards step make `Date.now()` go down between two calls, and code
    /// that subtracts two readings gets a negative duration.
    #[test]
    fn the_elapsed_time_source_never_goes_backwards() {
        let first = monotonic_millis();
        for _ in 0..1000 {
            assert!(monotonic_millis() >= first, "the monotonic source stepped backwards");
        }
    }

    /// The seed counts from the epoch and not from this process, which is what makes it VARY.
    ///
    /// **THE SEAM BESIDE IT IS THE TRAP.** `host_monotonic_ns` counts from the first call in this
    /// process, so it reads near zero at start-up on every run: wiring entropy to it compiles,
    /// looks wired, and seeds every run alike -- which is indistinguishable from not wiring it at
    /// all. And no reading taken INSIDE one run can tell the two apart, because an elapsed
    /// counter advances within a process exactly as a wall clock does. What separates them is the
    /// ORIGIN, so that is what is asserted.
    #[test]
    fn the_seed_counts_from_the_epoch_and_not_from_this_process() {
        const NANOS_AT_2020: u64 = 1_577_836_800_000_000_000;
        let first = seed();
        assert!(
            first > NANOS_AT_2020,
            "the seed is process-relative, so every run starts from the same place: {first}"
        );

        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(seed() > first, "the seed is fixed, so every run would seed alike");
    }

    /// And the seed reaches `Math.random`, which is what the verb was missing.
    ///
    /// An unseeded realm starts from a fixed constant deliberately -- the engine documents that as
    /// a stated deviation -- so a verb that installs output and a clock and stops there ships a
    /// `Math.random()` that returns the same sequence on every boot. The comparison is against
    /// THE VERB'S OWN REALM rather than one seeded here, so deleting the line from the verb is
    /// what turns this red.
    #[test]
    fn the_verbs_realm_moves_math_random_off_the_unseeded_sequence() {
        fn first_value(mut interpreter: Interpreter) -> String {
            match interpreter.run_source("Math.random()") {
                Ok(Completion::Normal(value)) => interpreter.describe(&value),
                other => panic!("Math.random() did not evaluate: {other:?}"),
            }
        }

        assert_ne!(
            first_value(Interpreter::new()),
            first_value(javascript_realm()),
            "the verb's realm did not install the seed, so Math.random() repeats across runs"
        );
    }

    #[test]
    fn the_language_comes_from_the_extension_and_an_unknown_one_names_the_known_ones() {
        assert_eq!(Language::of(Path::new("Program.cs")), Ok(Language::CSharp));
        assert_eq!(Language::of(Path::new("main.py")), Ok(Language::Python));
        assert_eq!(Language::of(Path::new("blink.js")), Ok(Language::JavaScript));
        let error = Language::of(Path::new("app.ts")).expect_err("refuses");
        assert!(error.contains(".cs") && error.contains(".py"), "got {error}");
        let bare = Language::of(Path::new("Makefile")).expect_err("refuses");
        assert!(bare.contains(".cs"), "a file with no extension is refused too: {bare}");

        let swift = Language::of(Path::new("greeting.swift")).expect_err("refuses");
        assert!(
            swift.contains("lamella flash") && swift.contains(".elf"),
            "an already-built image has somewhere to go and the refusal says where: {swift}"
        );
    }

    fn directives(source: &str) -> Result<(), String> {
        refuse_unhonored_directives(
            source,
            lamella_syntax::lexer::LexOptions { file_based: true, ..Default::default() },
        )
    }

    /// The compiler validates no directive name -- it hands `name` through untouched -- so this
    /// is the only thing between a misspelling and a program that builds without the dependency
    /// it asked for. Both shapes are refused, and they are told apart because the fixes differ.
    #[test]
    fn no_file_directive_is_accepted_and_ignored() {
        directives("class P { static void Main() { } }").expect("a file with none compiles");

        let typo = directives("#:pacakge Newtonsoft.Json\nclass P { static void Main() { } }")
            .expect_err("a misspelling is refused");
        assert!(typo.contains("Unrecognized directive 'pacakge'"), "named back: {typo}");
        assert!(!typo.contains("CS"), "no CS code on an SDK-layer error: {typo}");

        let known = directives("#:package Newtonsoft.Json\nclass P { static void Main() { } }")
            .expect_err("a recognized directive we do not honor is refused too");
        assert!(known.contains("#:package"), "names the directive: {known}");
        assert!(known.contains("feed"), "and what honoring it would take: {known}");
        assert!(
            !known.contains("Unrecognized"),
            "a directive we know is not a misspelling, and the fixes differ: {known}"
        );

        let shouted = directives("#:PROPERTY Lang=x\nclass P { static void Main() { } }")
            .expect_err("case matters");
        assert!(shouted.contains("Unrecognized directive 'PROPERTY'"), "verbatim: {shouted}");
    }

    /// A misplaced directive is CS9297 and one without the mode is CS9298 -- both the compiler's,
    /// with a precise code. Refusing here first would replace them with a vaguer sentence.
    #[test]
    fn a_file_that_did_not_lex_gets_the_compilers_diagnostics_rather_than_this_one() {
        let after_a_token = "using System;\n#:package Newtonsoft.Json\nclass P { }";
        assert!(
            directives(after_a_token).is_ok(),
            "the placement error belongs to the compiler, which reports CS9297 for it"
        );
    }

    /// The redirect for `--board` has one job: say the flag is gone and say what it did, without
    /// implying it reached hardware.
    #[test]
    fn the_removed_flag_is_redirected_rather_than_answered_with_unknown_option() {
        assert!(
            BOTH_MODES.contains("no longer an option"),
            "a flag that worked yesterday says where it went: {BOTH_MODES}"
        );
        assert!(
            BOTH_MODES.contains("fact table") && BOTH_MODES.contains("THIS machine"),
            "and what it actually was: {BOTH_MODES}"
        );
        assert!(
            !BOTH_MODES.lines().any(|line| line.trim_start().starts_with("--board <id>")),
            "it must not still be listed as a mode of this verb: {BOTH_MODES}"
        );
    }

    /// An assembly name goes into metadata and another assembly writes it down, so every path a
    /// user can type has to produce one -- including the shapes a path allows and a name does not.
    #[test]
    fn an_assembly_name_is_derived_from_the_file_and_is_always_an_identifier() {
        assert_eq!(assembly_name(Path::new("Program.cs")), "Program");
        assert_eq!(assembly_name(Path::new("/tmp/a b/My App.cs")), "My_App");
        assert_eq!(assembly_name(Path::new("blink.v2.cs")), "blink_v2");
        assert_eq!(assembly_name(Path::new("2048.cs")), "_2048", "a name cannot lead with a digit");
        assert_eq!(assembly_name(Path::new(".cs")), "_cs", "and can never come out empty");
    }

    /// **THE BOARD MODULE HAS TO TRAVEL WITH THE BINARY.** Serving `bsp/<board>/python/board.py`
    /// off disk would work only inside a checkout, which is exactly not where the person running
    /// a program against a board they do not own yet is standing.
    #[test]
    fn every_board_carries_its_generated_python_module() {
        assert!(
            BOARD_PYTHON.len() >= catalog::BOARDS.len(),
            "{} boards but {} python modules -- a board with none cannot be simulated",
            catalog::BOARDS.len(),
            BOARD_PYTHON.len()
        );
        let (_, text) = BOARD_PYTHON
            .iter()
            .find(|(id, _)| *id == "bbc-micro-bit-v2")
            .expect("the micro:bit v2 board module");
        assert!(!text.is_empty(), "an embedded module with no text serves nothing");
    }

    /// A named board resolves through the EMBEDDED table, not the working directory -- so the
    /// resolution works from anywhere. Checked by resolving with a current directory that has no
    /// `bsp/` in it at all.
    #[test]
    fn a_board_module_resolves_without_a_checkout_underneath() {
        let source = "import board\nprint(1)\n";
        let path = Path::new("nowhere/main.py");
        let bundle = compile_python(path, source, Some("bbc-micro-bit-v2"))
            .expect("a board module comes from the binary, not from bsp/ on disk");
        assert!(
            bundle.modules.iter().any(|module| module.name == "board"),
            "the bundle carries the board module: {:?}",
            bundle.modules.iter().map(|module| &module.name).collect::<Vec<_>>()
        );
    }

    /// Naming no board must not silently serve one.
    #[test]
    fn no_board_named_means_no_board_module() {
        let source = "import board\n";
        let compiled = compile_python(Path::new("nowhere/main.py"), source, None);
        if let Ok(bundle) = compiled {
            assert!(
                !bundle.modules.iter().any(|module| module.name == "board"),
                "a board module appeared without --board"
            );
        }
    }
    /// **A KIND OF FILE THE VERBS TAKE IS NAMED WHERE A READER LOOKS FOR IT**: the usage line a
    /// reader retypes, and the refusal met after naming a file the verbs do not take. Both took a
    /// project and neither said so.
    #[test]
    fn run_names_the_project_it_takes() {
        let first = RUN_USAGE.lines().next().unwrap_or_default();
        assert!(first.contains("file.csproj"), "the usage line: {first}");
        let refusal = Language::of(Path::new("app.ts")).expect_err("refuses");
        assert!(refusal.contains(".csproj"), "the refusal: {refusal}");
    }

    /// **A VERB WITH NO USAGE TEXT ANSWERS `--help` BY PRINTING NOTHING AND EXITING 0**, which
    /// reads to a person as "this tool has no help" and to a script as success.
    ///
    /// Asserting the FIRST LINE rather than the presence of a string also catches the likelier
    /// drift: a usage block copied from a neighbouring verb and not renamed.
    #[test]
    fn the_usage_opens_with_the_verb_it_belongs_to() {
        assert!(
            RUN_USAGE.starts_with("usage: lamella run"),
            "`run` must open with the line a reader retypes: {}",
            RUN_USAGE.lines().next().unwrap_or_default()
        );
        assert!(
            usage().starts_with("usage: lamella build"),
            "`build` must open with the line a reader retypes: {}",
            usage().lines().next().unwrap_or_default()
        );
    }

    /// **THE USAGE OFFERS EVERY FORMAT THE PARSER TAKES, BY THE NAME IT TAKES IT UNDER.** Every
    /// output `--format` parses -- each image format and the ELF -- must appear as a row of the
    /// usage, so one the parser gains cannot go unoffered.
    #[test]
    fn the_build_usage_offers_every_format_the_parser_takes() {
        let usage = usage();
        for output in lamella_flash_routes::artifact::Output::all() {
            assert!(
                usage
                    .lines()
                    .any(|line| line.split_whitespace().next() == Some(output.extension())),
                "no row offers `--format {}`:\n{usage}",
                output.extension()
            );
        }
    }

    /// **A DEBUG BUILD NAMES ITS SOURCE BY THE ABSOLUTE PATH AN EDITOR SENDS**, however the path
    /// was typed: relative, through `.` and `..`, or already absolute.
    #[test]
    fn a_debug_build_names_its_source_by_the_absolute_path_an_editor_sends() {
        let here = std::env::current_dir().expect("a working directory");
        let expected = here.join("samples").join("Program.cs");
        for typed in [
            "samples/Program.cs",
            "./samples/Program.cs",
            "samples/./Program.cs",
            "elsewhere/../samples/Program.cs",
            "samples/deeper/../../samples/Program.cs",
        ] {
            assert_eq!(
                document_path(Path::new(typed)).expect("an absolute path"),
                expected.display().to_string(),
                "typed as {typed}"
            );
        }
        assert_eq!(
            document_path(&expected).expect("an absolute path"),
            expected.display().to_string(),
            "an absolute path is already the one an editor sends"
        );
    }

    /// **`.` AND `..` ARE RESOLVED FROM THE PATH ALONE**, including where the path is already
    /// absolute -- which is where POSIX's `std::path::absolute` leaves a `..` in place -- and a
    /// `..` at the root stays at the root.
    #[test]
    fn dot_segments_are_resolved_from_the_path_alone() {
        let here = std::env::current_dir().expect("a working directory");
        let dotted = here
            .join("elsewhere")
            .join("..")
            .join("samples")
            .join("deeper")
            .join("..")
            .join("Program.cs");
        assert_eq!(
            without_dot_segments(&dotted),
            here.join("samples").join("Program.cs")
        );
        let root = here.ancestors().last().expect("a root").to_path_buf();
        assert_eq!(
            without_dot_segments(&root.join("..").join("..").join("Program.cs")),
            root.join("Program.cs")
        );
    }

    /// **`--format elf` IS BUILT ON THE CLASS-LIBRARY TIER AND REFUSED BY NAME ON THE FLAT ONE**: the
    /// refusal names the flag that makes it work and offers, on a line of its own, exactly the
    /// formats the flat tier's image can be written in -- which the ELF is not one of.
    #[test]
    fn an_elf_on_the_flat_tier_names_the_flag_and_the_formats_that_work() {
        assert_eq!(elf_refusal(crate::flash::Tier::ClassLibrary), None);
        let message =
            elf_refusal(crate::flash::Tier::Flat).expect("the flat tier refuses an ELF");
        assert!(
            message.contains(&format!("Add {},", crate::flash::CLASS_LIBRARY_FLAG)),
            "the refusal must name the flag that makes the ELF work:\n{message}"
        );
        let listing = format!("    {}", lamella_flash_routes::artifact::Format::listing());
        assert!(
            message.lines().any(|line| line == listing),
            "the refusal must offer the image formats as the table lists them:\n{message}"
        );
        assert!(
            !listing.contains("elf"),
            "the flat tier cannot write the ELF, so it must not be offered:\n{message}"
        );
        crate::rendered::assert_renders_cleanly(&message, crate::rendered::four_space_sample);
        for line in message.lines() {
            assert!(line.len() <= 100, "a line runs to {}:\n{message}", line.len());
        }
    }

    /// `run` has three modes and each serves a different language set, so the usage has to state
    /// them and must not describe a question another verb answers.
    #[test]
    fn the_run_usage_states_each_modes_language_and_does_not_claim_to_answer_fit() {
        assert!(
            !RUN_USAGE.contains("would FIT"),
            "`run --board` does not answer fit -- `build --board` does:\n{RUN_USAGE}"
        );
        for (flag, language) in [("--target <t>", "C#")] {
            let paragraph = RUN_USAGE
                .split("\n\n")
                .find(|block| block.starts_with(flag))
                .unwrap_or_else(|| panic!("no paragraph opens with `{flag}`:\n{RUN_USAGE}"));
            assert!(
                paragraph.contains(language),
                "`{flag}` serves {language} only today and the usage does not say so:\n{paragraph}"
            );
        }
    }

}
