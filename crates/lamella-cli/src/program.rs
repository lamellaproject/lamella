//! `lamella run` and `lamella build`: a program, in whichever language it is written.

use crate::args::{self, Spec};
use lamella_catalog::{self as catalog, BOARD_PYTHON};
use lamella_bsp_gen::fit::fit;
use lamella_wire_host::engine::{LcscCompiler, LoopbackLink, Outcome, Repl};
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
                "{}: lamella run and lamella build read .cs (C#), .py (Python) and .js (JavaScript).\n\n\
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
usage: lamella run <file.cs|file.py|file.js> [--target <t>]

Compiles and runs the program, and STAYS until it ends -- its output appears here as it is
printed. A program written to loop forever runs until you stop this tool.

With neither option it runs on this machine, which needs no hardware and is the fastest way to
find out whether a program compiles and does what you meant.

--target <t> runs it ON a board that already has firmware, with the output still appearing here.
`lamella devices` prints the word to pass. A cycle is about a second, and the board keeps its
firmware. C# only: a Python or JavaScript program runs on this machine.

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

    let (language, source) = match read(&path) {
        Ok(read) => read,
        Err(error) => {
            eprintln!("lamella run: {error}");
            return ExitCode::FAILURE;
        }
    };
    match language {
        Language::CSharp => run_csharp(&source),
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
/// hardware belongs on `flash`, and the diagnostic below says so where it comes up.
fn run_csharp(source: &str) -> ExitCode {
    let compiler = match LcscCompiler::discover() {
        Ok(compiler) => compiler,
        Err(error) => {
            eprintln!("lamella run: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(corlib) = compiler.references().first().cloned() else {
        eprintln!("lamella run: the compiler found no reference assemblies");
        return ExitCode::FAILURE;
    };
    let mut repl = Repl::new(Box::new(compiler), Box::new(LoopbackLink::new(corlib)));
    match repl.eval_program(source) {
        Ok(Outcome::Ran { output, exit, .. }) => {
            print!("{output}");
            if exit == 0 {
                ExitCode::SUCCESS
            } else {
                eprintln!("lamella run: the program exited {exit}");
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
                     \x20   lamella flash <file> --board <id> --unsafe"
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
                "lamella board-module-run: {} is JavaScript. This path serves a generated `board`                  MODULE, which is
Python's shape; this JavaScript profile has no module loader at \
                 all, so nothing could import it.",
                path.display()
            );
            ExitCode::FAILURE
        }
    }
}

/// `lamella build <file> [--board <id>] [--out <path>]`: produce the artifact a device runs.
pub fn build_command(args: &[String]) -> ExitCode {
    let spec =
        Spec { verb: "build", usage: Some(USAGE), values: &["--board", "--out", "--format"], flags: &["--unsafe"] };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    let path = match parsed.only_positional("build", "source file") {
        Ok(path) => Path::new(path).to_path_buf(),
        Err(error) => {
            eprintln!("{error}\n\n{USAGE}");
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

    if let Some(name) = parsed.value("--format") {
        return build_flashable(
            &path,
            &source,
            language,
            name,
            parsed.value("--board"),
            parsed.value("--out"),
            parsed.flag("--unsafe"),
        );
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
    println!("  {}  {} B", built.what, built.bytes.len());

    let Some(board_id) = parsed.value("--board") else {
        return ExitCode::SUCCESS;
    };
    answer_fit(board_id, &built)
}

const USAGE: &str = "\
usage: lamella build <file.cs|file.py> [--board <id>] [--format <f>] [--out <path>]

With --format, it builds the BARE-METAL IMAGE for --board and writes it in that format -- which is
exactly what `lamella flash` takes, so `build` produces what `flash` consumes and neither has to
touch hardware. Formats: bin, hex (Intel HEX), s19 (Motorola S-records).

Without --format it builds the ordinary artifact -- an assembly, a baked image, or a Python bundle
-- and with --board it also answers whether that fits.
";

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
) -> ExitCode {
    let format = match lamella_flash_routes::artifact::Format::parse(format_name) {
        Ok(format) => format,
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

    let (image, base) = match crate::flash::image_for_board(path, source, board_id, unsafe_code) {
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
    println!("\nwrite it with:\n    lamella flash {} --board {board_id}", out.display());
    ExitCode::SUCCESS
}

/// What a build produced.
struct Built {
    /// The artifact.
    bytes: Vec<u8>,
    /// The extension it is conventionally written with.
    extension: &'static str,
    /// What the artifact IS, in the terms the rest of the toolchain uses for it.
    what: &'static str,
    /// **WHAT THE BYTE COUNT DOES NOT INCLUDE, AND THEREFORE WHAT A FIT VERDICT OVER IT MEANS.**
    ///
    /// A fit verdict compares a number against a board's whole flash budget, which is the right
    /// comparison only when the artifact is the whole flash occupant. For a tier where the board
    /// is already running firmware that the image is loaded INTO, it is not -- the headroom is an
    /// upper bound rather than the space the image will have. Carried with the artifact rather
    /// than reconstructed at the comparison, so the caveat cannot be attached to the wrong tier.
    excludes: &'static str,
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
    let assembly = compile_csharp_assembly(path, source, unsafe_code)?;
    #[cfg(feature = "bake")]
    {
        let image = crate::bake::bake(assembly)?;
        return Ok(Built {
            bytes: image,
            extension: "lmli",
            what: "baked flash image",
            excludes: "the serve firmware already resident on the board, which this image is \
                       loaded INTO rather than replacing",
        });
    }
    #[cfg(not(feature = "bake"))]
    Ok(Built {
        bytes: assembly,
        extension: "dll",
        what: "assembly (NOT a flash image -- this build has no `bake` feature)",
        excludes: "everything the device supplies -- this is the assembly, not an image. \
                   Build the tool with `--features bake` for the flash image a board runs",
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
) -> Result<Vec<u8>, String> {
    let compiler = LcscCompiler::discover()?;
    let references: Vec<lamella_metadata::Assembly> = compiler
        .references()
        .iter()
        .filter_map(|bytes| lamella_metadata::Assembly::read(bytes).ok())
        .collect();
    let name = assembly_name(path);
    let options = lamella_syntax::lexer::LexOptions {
        unsafe_code,
        file_based: true,
        ..Default::default()
    };
    refuse_unhonored_directives(source, options.clone())?;
    let compiled = lamella_assemble::compile_source_with(
        source,
        &path.display().to_string(),
        &name,
        &name,
        &references,
        false,
        options,
    );
    match compiled.image {
        Some(image) => Ok(image),
        None => Err(render_diagnostics(&compiled)),
    }
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

/// Render a failed compilation the way the toolchain's other front ends do: one `CSnnnn` line per
/// diagnostic, or the emit error when binding was clean and a construct is not lowered.
fn render_diagnostics(compiled: &lamella_assemble::Compilation) -> String {
    if let Some(emit_error) = &compiled.emit_error {
        return format!("{emit_error:?}");
    }
    let mut text = String::new();
    for diagnostic in &compiled.diagnostics {
        if !text.is_empty() {
            text.push('\n');
        }
        let severity = if diagnostic.is_error() { "error" } else { "warning" };
        text.push_str(&format!(
            "CS{:04}: {severity}: {}",
            diagnostic.code, diagnostic.message
        ));
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
        what: "Python bundle",
        excludes: "the Python interpreter firmware, which is by far the larger half and is \
                   already on the board",
    })
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
            .find(|(id, _)| *id == "micro-bit-v2")
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
        let bundle = compile_python(path, source, Some("micro-bit-v2"))
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
            USAGE.starts_with("usage: lamella build"),
            "`build` must open with the line a reader retypes: {}",
            USAGE.lines().next().unwrap_or_default()
        );
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
