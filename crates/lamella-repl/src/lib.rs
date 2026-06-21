//! A host-PC C# REPL on the lamella interpreter, bootstrap-compiled with csc.

use lamella_load::{load, load_library};
use lamella_metadata::Assembly;
use lamella_ves::{MethodId, Module, ObjectRef, Vm, run};
use std::path::{Path, PathBuf};

pub use lamella_ves::Value;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};

/// Evaluates one C# expression line, returning the program's console output on
/// success or a diagnostic/trap message on failure.
///
/// The pipeline is: wrap the line as a `WriteLine` of the expression, write it to
/// a temp `.cs`, compile it with csc into a temp `.dll`, then load + run that on a
/// fresh interpreter and return [`Vm::output_string`]. A csc failure returns its
/// diagnostics as `Err`; an interpreter trap returns the trap text as `Err`. Temp
/// files are removed before returning, on every path.
///
/// # Errors
///
/// Returns `Err` when csc cannot be located, when compilation fails (the csc
/// diagnostics), when the produced assembly cannot be read or loaded, or when the
/// interpreter traps while running it.
pub fn eval(source_line: &str) -> Result<String, String> {
    let tools = Toolchain::discover()?;
    let work = TempProgram::new()?;
    let result = eval_in(source_line, &tools, &work);
    work.cleanup();
    result
}

/// The body of [`eval`], split out so [`TempProgram::cleanup`] runs whether this
/// returns `Ok` or `Err`.
fn eval_in(source_line: &str, tools: &Toolchain, work: &TempProgram) -> Result<String, String> {
    fs::write(&work.source, wrap_expression(source_line))
        .map_err(|error| format!("cannot write temp source {}: {error}", work.source.display()))?;

    compile(tools, work, CompileTarget::Exe)?;

    let bytes = fs::read(&work.assembly)
        .map_err(|error| format!("cannot read compiled assembly: {error}"))?;
    let assembly =
        Assembly::read(&bytes).map_err(|error| format!("cannot read metadata: {error:?}"))?;
    let program = load(&assembly).map_err(|error| format!("cannot load: {error}"))?;

    let mut vm = Vm::new();
    match run(&program.module, &mut vm, program.entry, Vec::new()) {
        Ok(_) => Ok(vm.output_string()),
        Err(trap) => Err(format!("trap: {trap}")),
    }
}

/// Wraps a single C# expression as a minimal compilable program that prints it.
///
/// The line is spliced verbatim as the argument to `Console.WriteLine`, so it must
/// be an expression (`1 + 2`, `"hi".Length`, `System.Math.Max(3, 7)`), not a
/// statement or a declaration.
///
fn wrap_expression(source_line: &str) -> String {
    format!(
        "using System;\n\
         public class __Repl {{ public static int Main() {{ \
         System.Console.WriteLine({});\n\
         return 0; }} }}\n",
        source_line.trim_end()
    )
}

/// Whether csc emits an executable (a real entry point is run) or a library (a class
/// loaded for its members, with no entry point). [`eval`] wraps an expression in a `Main`
/// and runs the entry, so it compiles [`CompileTarget::Exe`]; the stateful [`ReplSession`]
/// emits an entry-point-free `__Repl` class and runs `__Submit` by name, so it compiles
/// [`CompileTarget::Library`] and loads via [`load_library`].
#[derive(Clone, Copy)]
enum CompileTarget {
    Exe,
    Library,
}

impl CompileTarget {
    /// The csc `/target:` switch this target selects.
    fn switch(self) -> &'static str {
        match self {
            CompileTarget::Exe => "/target:exe",
            CompileTarget::Library => "/target:library",
        }
    }
}

/// Invokes csc on the temp source, producing the temp assembly as `target` (an executable
/// for [`eval`], a library for [`ReplSession`]). Returns the csc diagnostics as `Err` if
/// csc exits nonzero or the assembly is missing.
fn compile(tools: &Toolchain, work: &TempProgram, target: CompileTarget) -> Result<(), String> {
    let mut command = Command::new(&tools.dotnet);
    command
        .arg(&tools.csc)
        .args(["/nologo", "/nostdlib", target.switch()])
        .arg(format!("/out:{}", work.assembly.display()));
    for reference in &tools.references {
        command.arg(format!("/reference:{}", reference.display()));
    }
    command.arg(&work.source);

    let output = command
        .output()
        .map_err(|error| format!("cannot run csc ({}): {error}", tools.csc.display()))?;
    if output.status.success() && work.assembly.exists() {
        return Ok(());
    }
    let mut diagnostics = String::from_utf8_lossy(&output.stdout).into_owned();
    diagnostics.push_str(&String::from_utf8_lossy(&output.stderr));
    let diagnostics = diagnostics.trim_end();
    if diagnostics.is_empty() {
        Err(format!("csc failed with {}", output.status))
    } else {
        Err(diagnostics.to_owned())
    }
}

/// Located host tools: the `dotnet` launcher, the Roslyn `csc.dll`, and the
/// reference assemblies to compile against.
struct Toolchain {
    dotnet: PathBuf,
    csc: PathBuf,
    references: Vec<PathBuf>,
}

impl Toolchain {
    fn discover() -> Result<Toolchain, String> {
        let csc = match env::var_os("LAMELLA_CSC") {
            Some(path) => PathBuf::from(path),
            None => latest_match(
                "C:\\Program Files\\dotnet\\sdk",
                &["Roslyn", "bincore", "csc.dll"],
            )
            .ok_or_else(|| {
                "cannot find csc.dll under C:\\Program Files\\dotnet\\sdk; \
                 set LAMELLA_CSC to its path"
                    .to_owned()
            })?,
        };

        let ref_dir = match env::var_os("LAMELLA_REF_DIR") {
            Some(path) => PathBuf::from(path),
            None => latest_match(
                "C:\\Program Files\\dotnet\\packs\\Microsoft.NETCore.App.Ref",
                &["ref", "net8.0"],
            )
            .ok_or_else(|| {
                "cannot find the net8.0 reference pack under \
                 C:\\Program Files\\dotnet\\packs\\Microsoft.NETCore.App.Ref; \
                 set LAMELLA_REF_DIR to its directory"
                    .to_owned()
            })?,
        };
        let references = reference_dlls(&ref_dir)?;

        let dotnet = env::var_os("LAMELLA_DOTNET")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("dotnet"));

        Ok(Toolchain {
            dotnet,
            csc,
            references,
        })
    }
}

/// Collects every `*.dll` in the reference-pack directory, as the `/reference:`
/// list to give csc.
fn reference_dlls(ref_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut references = Vec::new();
    let entries = fs::read_dir(ref_dir)
        .map_err(|error| format!("cannot read ref dir {}: {error}", ref_dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("dll")) {
            references.push(path);
        }
    }
    if references.is_empty() {
        return Err(format!(
            "no reference assemblies (*.dll) found in {}",
            ref_dir.display()
        ));
    }
    references.sort();
    Ok(references)
}

fn latest_match(root: &str, tail: &[&str]) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|version_dir| {
            let mut candidate = version_dir.clone();
            candidate.extend(tail);
            candidate.exists()
        })
        .collect();
    candidates.sort();
    let version_dir = candidates.pop()?;
    let mut full = version_dir;
    full.extend(tail);
    Some(full)
}

/// A unique pair of temp paths (`<base>.cs` and `<base>.dll`) for one submission,
/// deleted by [`TempProgram::cleanup`].
struct TempProgram {
    source: PathBuf,
    assembly: PathBuf,
}

impl TempProgram {
    /// Reserves a fresh, process-and-call-unique base name in the system temp dir.
    fn new() -> Result<TempProgram, String> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = env::temp_dir().join(format!(
            "lamella-repl-{}-{nanos}-{seq}",
            std::process::id()
        ));
        Ok(TempProgram {
            source: base.with_extension("cs"),
            assembly: base.with_extension("dll"),
        })
    }

    /// Removes the temp source and assembly, ignoring errors (best effort).
    fn cleanup(&self) {
        let _ = fs::remove_file(&self.source);
        let _ = fs::remove_file(&self.assembly);
    }
}

/// A persistent-state REPL session: ONE interpreter, ONE heap, and ONE `__Repl`
/// instance kept alive across submissions, so declared state survives line to line.
///
pub struct Session {
    /// The one interpreter context: the heap the instance lives on, plus statics.
    vm: Vm,
    /// The loaded `__Repl` module: its methods (resolved by name in
    /// [`Session::run_submission`]) and the type layout the instance was built from.
    module: Module,
    /// The persistent `__Repl` instance -- the receiver every submission runs against.
    /// Kept current across collections by re-reading it from its static root slot.
    instance: ObjectRef,
    /// The static-field slot rooting `instance` for the collector: one past the module's
    /// own static fields, so it never collides with program state (see the type docs).
    root_slot: usize,
}

impl Session {
    /// Opens a persistent session over the `__Repl` class in the assembly at `path`:
    /// loads it, allocates the single `__Repl` instance, runs its parameterless `.ctor`
    /// against that instance, and roots it for the collector.
    ///
    /// The assembly must define `class __Repl` with a parameterless instance `.ctor`. It
    /// needs no entry point: [`load_library`] binds the class without one, and a session
    /// runs named methods rather than `Main`. (The hand-assembled fixture still carries a
    /// trivial `Main`, which `load_library` simply does not treat as an entry.)
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read, is not a valid managed assembly, fails
    /// to load, declares no `__Repl..ctor`, or the constructor traps.
    pub fn open(path: &Path) -> Result<Session, String> {
        let bytes =
            fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let assembly =
            Assembly::read(&bytes).map_err(|error| format!("cannot read metadata: {error:?}"))?;
        let module = load_library(&assembly).map_err(|error| format!("cannot load: {error}"))?;

        let ctor = find_method(&module, "__Repl..ctor")
            .ok_or_else(|| "assembly defines no __Repl..ctor".to_owned())?;
        let type_id = module
            .method_type(ctor)
            .ok_or_else(|| "__Repl..ctor has no declaring type".to_owned())?;
        let fields = module
            .type_field_defaults(type_id)
            .ok_or_else(|| "__Repl has no recorded field layout".to_owned())?
            .to_vec();

        let root_slot = module.static_field_defaults().len();
        let mut storage = module.static_field_defaults().to_vec();
        storage.push(Value::Null);

        let mut vm = Vm::new();
        vm.init_statics(&storage);
        let instance = vm.heap_mut().alloc_instance(type_id, fields);
        vm.set_static_field(root_slot, Value::Object(instance));

        run(&module, &mut vm, ctor, vec![Value::Object(instance)])
            .map_err(|trap| format!("trap running __Repl..ctor: {trap}"))?;
        let instance = current_instance(&vm, root_slot)?;

        Ok(Session {
            vm,
            module,
            instance,
            root_slot,
        })
    }

    /// Runs the named submission method of `__Repl` against the persistent instance and
    /// returns the console output it produced.
    ///
    /// `method` is the qualified name the loader records, e.g. `"__Repl.SetX"`. The method
    /// is run with the persistent instance as `this` (argument 0), so it reads and writes
    /// the SAME fields earlier submissions did -- that is the state persistence this proves.
    /// The captured console output is returned (a submission that prints its result, as the
    /// compiler's lowering will, surfaces the value this way).
    ///
    /// # Errors
    /// Returns `Err` if `__Repl` declares no method named `method`, or the method traps.
    pub fn run_submission(&mut self, method: &str) -> Result<String, String> {
        let id = find_method(&self.module, method)
            .ok_or_else(|| format!("__Repl declares no method named {method:?}"))?;
        let before = self.vm.output().len();
        run(
            &self.module,
            &mut self.vm,
            id,
            vec![Value::Object(self.instance)],
        )
        .map_err(|trap| format!("trap running {method}: {trap}"))?;
        self.instance = current_instance(&self.vm, self.root_slot)?;
        Ok(String::from_utf16_lossy(&self.vm.output()[before..]))
    }

    /// The value of instance field `slot` of the persistent `__Repl`, for tests/inspection
    /// (the durable state itself, read straight off the heap).
    #[must_use]
    pub fn instance_field(&self, slot: u32) -> Option<Value> {
        self.vm.heap().instance_field(self.instance, slot)
    }
}

/// One accumulated field of the growing `__Repl` class: a declared variable that must
/// persist across submissions, as `(type, name)` in stable append order.
///
/// The slot a field occupies in the instance is its index in [`ReplSession::fields`] --
/// a NEW name is only ever appended (never reordered or removed), so a value migrated into
/// slot `i` of a newly emitted instance is the same variable it was in the prior one. A
/// REDEFINITION of an existing name reuses that name's slot in place (see
/// [`ReplSession::apply_declarations`]) rather than appending, so a resubmitted `int x = ...;`
/// overwrites the field csc already emitted instead of declaring a duplicate it would reject.
#[derive(Clone)]
struct ReplField {
    /// The C# type as written in the source (`int`, `string`, `System.Int32`, `int[]`).
    type_text: String,
    /// The variable name.
    name: String,
}

/// What a submission line is, and the C# statement(s) to splice into `__Submit`.
enum Submission {
    /// A declaration `<type> <name> = <init>;` (or a multi-declarator
    /// `<type> a = 1, b = 2;`) -- records one persistent field per declarator, all sharing
    /// the leading type, and the body assigns each initializer to its field
    /// (`a = 1; b = 2;`). A declarator without an initializer (`int a;`) records the field
    /// and contributes no assignment, so it simply takes its type's zero default.
    Declaration {
        /// The new fields this declaration introduces, in source order, each `(type, name)`.
        decls: Vec<ReplField>,
        body: String,
    },
    /// A statement (ends with `;`): spliced verbatim, declares no new field.
    Statement { body: String },
    /// A bare expression: printed via `System.Console.WriteLine(<expr>);`.
    Expression { body: String },
}

/// A stateful host-PC C# REPL built on the csc bootstrap: declarations persist across
/// submissions, so `int x = 5;` then `x * 2` prints `10`.
///
pub struct ReplSession {
    /// The located csc toolchain, discovered once and reused for every submission.
    tools: Toolchain,
    /// The accumulated persistent field declarations, in stable append order. The index
    /// of a field is the instance slot it occupies (see [`ReplField`]).
    fields: Vec<ReplField>,
    /// How many submissions have been compiled, for a unique `__Submit`-free temp name
    /// per turn (the class is re-emitted each turn; only the temp paths must differ).
    counter: u64,
    /// The interpreter + heap carrying the current `__Repl` instance. `None` until the
    /// first submission (the first compile establishes the type and the instance).
    state: Option<ReplState>,
}

/// The carried-over state between submissions, just enough to MIGRATE forward: the
/// interpreter holding the prior `__Repl` instance, and that instance's handle.
///
struct ReplState {
    vm: Vm,
    instance: ObjectRef,
}

impl ReplSession {
    /// Creates a stateful REPL session, discovering the csc toolchain up front.
    ///
    /// # Errors
    /// Returns `Err` if csc / the reference pack cannot be located (so a caller can
    /// skip-with-note exactly as the stateless tests do).
    pub fn new() -> Result<ReplSession, String> {
        let tools = Toolchain::discover()?;
        Ok(ReplSession {
            tools,
            fields: Vec::new(),
            counter: 0,
            state: None,
        })
    }

    /// Submits one REPL line: classifies it, re-emits the growing `__Repl` class, compiles
    /// and loads it, migrates prior field values into a fresh instance (stable slot order),
    /// runs the line's `__Submit`, and returns the console output it produced.
    ///
    /// A pure declaration or statement returns an empty string (it prints nothing); a bare
    /// expression is wrapped in `WriteLine`, so its output ends with a newline. State set by
    /// an earlier submission is visible to a later one because the field carrying it is
    /// migrated by slot into each newly emitted instance.
    ///
    /// # Errors
    /// Returns `Err` if csc rejects the emitted program (its diagnostics), the produced
    /// assembly cannot be read / loaded, the emitted `__Repl` is missing its expected
    /// members, or the interpreter traps running `__Submit`.
    pub fn submit(&mut self, line: &str) -> Result<String, String> {
        let submission = classify(line);

        let prior_field_count = self.fields.len();
        let fields_snapshot = self.fields.clone();
        let reset_slots = if let Submission::Declaration { decls, .. } = &submission {
            self.apply_declarations(decls)
        } else {
            Vec::new()
        };

        let source = self.emit_class(submission.body());
        let work = TempProgram::new()?;
        self.counter += 1;
        let result = self.submit_in(&source, prior_field_count, &reset_slots, &work);
        work.cleanup();
        if result.is_err() {
            self.fields = fields_snapshot;
        }
        result
    }

    /// Folds one line's declarations into the accumulated field list and returns the slots
    /// whose value must NOT be migrated from the prior instance (because their type changed).
    ///
    /// For each declarator `(type, name)`:
    /// - a NEW name appends a field (a new slot past the prior ones, taking its zero default);
    /// - a REDEFINITION of an existing name with the SAME type reuses that slot unchanged --
    ///   the prior value migrates in and `__Submit`'s assignment overwrites it;
    /// - a REDEFINITION with a DIFFERENT type retypes that slot and marks it for reset, so the
    ///   old (differently typed) value is dropped to the new type's default rather than migrated.
    ///
    /// Reusing the slot of a resubmitted name is what keeps the re-emitted class valid: csc
    /// rejects two fields of the same name, so a duplicate append would fail to compile.
    fn apply_declarations(&mut self, decls: &[ReplField]) -> Vec<usize> {
        let mut reset_slots = Vec::new();
        for decl in decls {
            match self.fields.iter().position(|field| field.name == decl.name) {
                Some(slot) => {
                    if self.fields[slot].type_text != decl.type_text {
                        self.fields[slot].type_text = decl.type_text.clone();
                        reset_slots.push(slot);
                    }
                }
                None => self.fields.push(decl.clone()),
            }
        }
        reset_slots
    }

    /// The body of [`ReplSession::submit`], split out so [`TempProgram::cleanup`] runs on
    /// every path. Writes the emitted source, compiles + loads it, migrates the prior
    /// instance's fields into a fresh one, runs `__Submit`, and adopts the new state.
    fn submit_in(
        &mut self,
        source: &str,
        prior_field_count: usize,
        reset_slots: &[usize],
        work: &TempProgram,
    ) -> Result<String, String> {
        fs::write(&work.source, source).map_err(|error| {
            format!("cannot write temp source {}: {error}", work.source.display())
        })?;
        compile(&self.tools, work, CompileTarget::Library)?;

        let bytes = fs::read(&work.assembly)
            .map_err(|error| format!("cannot read compiled assembly: {error}"))?;
        let assembly =
            Assembly::read(&bytes).map_err(|error| format!("cannot read metadata: {error:?}"))?;
        let module = load_library(&assembly).map_err(|error| format!("cannot load: {error}"))?;

        let ctor = find_method(&module, "__Repl..ctor")
            .ok_or_else(|| "emitted assembly defines no __Repl..ctor".to_owned())?;
        let type_id = module
            .method_type(ctor)
            .ok_or_else(|| "__Repl..ctor has no declaring type".to_owned())?;
        let mut fields = module
            .type_field_defaults(type_id)
            .ok_or_else(|| "__Repl has no recorded field layout".to_owned())?
            .to_vec();

        if let Some(prior) = &self.state {
            for slot in 0..prior_field_count {
                if reset_slots.contains(&slot) {
                    continue;
                }
                if let Some(value) = prior.vm.heap().instance_field(prior.instance, slot as u32) {
                    if let Some(target) = fields.get_mut(slot) {
                        *target = value;
                    }
                }
            }
        }

        let root_slot = module.static_field_defaults().len();
        let mut storage = module.static_field_defaults().to_vec();
        storage.push(Value::Null);

        let mut vm = Vm::new();
        vm.init_statics(&storage);
        let instance = vm.heap_mut().alloc_instance(type_id, fields);
        vm.set_static_field(root_slot, Value::Object(instance));

        let submit = find_method(&module, "__Repl.__Submit")
            .ok_or_else(|| "emitted __Repl defines no __Submit".to_owned())?;
        let before = vm.output().len();
        run(&module, &mut vm, submit, vec![Value::Object(instance)])
            .map_err(|trap| format!("trap running __Submit: {trap}"))?;
        let instance = current_instance(&vm, root_slot)?;
        let output = String::from_utf16_lossy(&vm.output()[before..]);

        self.state = Some(ReplState { vm, instance });
        Ok(output)
    }

    /// Emits the full `__Repl` class for this turn: every accumulated field declared bare
    /// (its value comes from migration / `__Submit`, not a field initializer) and `__Submit`
    /// carrying this line's body. No `Main` -- the class is compiled `/target:library` and
    /// loaded via [`load_library`], which neither requires nor runs an entry point.
    fn emit_class(&self, body: &str) -> String {
        let mut source = String::from("using System;\npublic class __Repl {\n");
        for field in &self.fields {
            source.push_str("    public ");
            source.push_str(&field.type_text);
            source.push(' ');
            source.push_str(&field.name);
            source.push_str(";\n");
        }
        source.push_str("    public void __Submit() {\n        ");
        source.push_str(body);
        source.push_str("\n    }\n}\n");
        source
    }
}

impl Submission {
    /// The C# statement(s) this submission splices into `__Submit`'s body.
    fn body(&self) -> &str {
        match self {
            Submission::Declaration { body, .. }
            | Submission::Statement { body }
            | Submission::Expression { body } => body,
        }
    }
}

/// Classifies one REPL line into a [`Submission`] (v1.1 heuristic).
///
fn classify(line: &str) -> Submission {
    let trimmed = line.trim();
    if let Some((decls, body)) = parse_declaration(trimmed) {
        return Submission::Declaration { decls, body };
    }
    if trimmed.ends_with(';') {
        return Submission::Statement {
            body: trimmed.to_owned(),
        };
    }
    Submission::Expression {
        body: format!("System.Console.WriteLine({});", trimmed.trim_end()),
    }
}

/// If `line` is a local-variable declaration, returns its new fields (one per declarator) and
/// the `__Submit` body that assigns each initializer to its field; otherwise `None`.
///
/// Shape: a leading type token (dotted name, one optional `<...>` generic list, optional
/// trailing `[]`), mandatory whitespace, then a `;`-terminated comma-list of declarators, each
/// `<name>` or `<name> = <init>`. A declarator with no initializer contributes a field but no
/// assignment (it keeps its type's zero default). The body for `int a = 1, b = 2;` is
/// `a = 1; b = 2;` -- each field is declared bare in `emit_class`, so the body only assigns.
fn parse_declaration(line: &str) -> Option<(Vec<ReplField>, String)> {
    if !line.ends_with(';') {
        return None;
    }
    let (type_text, after_type) = split_type_token(line)?;
    if !is_plausible_type(type_text) {
        return None;
    }
    let declarators = after_type.strip_suffix(';')?.trim_end();
    if declarators.is_empty() {
        return None;
    }

    let mut fields = Vec::new();
    let mut assignments = String::new();
    for declarator in split_top_level_commas(declarators) {
        let (name, initializer) = split_declarator(declarator)?;
        fields.push(ReplField {
            type_text: type_text.to_owned(),
            name: name.to_owned(),
        });
        if let Some(initializer) = initializer {
            if !assignments.is_empty() {
                assignments.push(' ');
            }
            assignments.push_str(name);
            assignments.push_str(" = ");
            assignments.push_str(initializer.trim());
            assignments.push(';');
        }
    }
    if fields.is_empty() {
        return None;
    }
    Some((fields, assignments))
}

/// Parses the leading type token of `line`: a run of identifier/`.` chars, then an optional
/// single (non-nested) `<...>` generic list and an optional trailing `[]`, and requires that
/// at least one whitespace separates it from what follows. Returns `(type_text, rest)` where
/// `rest` is the trimmed remainder after that whitespace, or `None` if the shape does not hold.
fn split_type_token(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let is_type_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'.';

    let mut i = 0;
    while i < bytes.len() && is_type_char(bytes[i]) {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    if i < bytes.len() && bytes[i] == b'<' {
        match line[i..].find('>') {
            Some(offset) => i += offset + 1,
            None => return None,
        }
    }
    if i + 1 < bytes.len() && bytes[i] == b'[' && bytes[i + 1] == b']' {
        i += 2;
    }
    let type_end = i;

    let whitespace = bytes[type_end..]
        .iter()
        .take_while(|b| b.is_ascii_whitespace())
        .count();
    if whitespace == 0 || type_end + whitespace >= bytes.len() {
        return None;
    }
    Some((&line[..type_end], line[type_end + whitespace..].trim_start()))
}

/// Whether `type_text` can plausibly be a variable's type for hoisting. Rejects the inferred
/// types (`var`, `dynamic`) and any leading reserved keyword that is not a built-in value/ref
/// type keyword -- so statement keywords like `return`/`throw`/`if`/`new`/`using` never get
/// mistaken for a type. A non-keyword identifier or dotted/generic/array name is accepted as a
/// user type (`Foo`, `System.Int32`, `List<int>`, `int[]`).
fn is_plausible_type(type_text: &str) -> bool {
    let head = type_text
        .split(['.', '<', '['])
        .next()
        .unwrap_or(type_text);
    if head.is_empty() {
        return false;
    }
    if matches!(head, "var" | "dynamic") {
        return false;
    }
    const TYPE_KEYWORDS: &[&str] = &[
        "bool", "byte", "sbyte", "char", "decimal", "double", "float", "int", "uint", "long",
        "ulong", "short", "ushort", "string", "object", "void", "nint", "nuint",
    ];
    if TYPE_KEYWORDS.contains(&head) {
        return true;
    }
    !is_reserved_keyword(head)
}

/// Whether `word` is a C# reserved keyword (the ones that, as a leading token, mean the line is
/// a statement/directive rather than a declaration -- so `return x;` is not read as a decl).
/// Built-in type keywords are intentionally NOT listed here; [`is_plausible_type`] admits those.
fn is_reserved_keyword(word: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "abstract", "as", "base", "break", "case", "catch", "checked", "class", "const",
        "continue", "default", "delegate", "do", "else", "enum", "event", "explicit", "extern",
        "false", "finally", "fixed", "for", "foreach", "goto", "if", "implicit", "in", "interface",
        "internal", "is", "lock", "namespace", "new", "null", "operator", "out", "override",
        "params", "private", "protected", "public", "readonly", "ref", "return", "sealed",
        "sizeof", "stackalloc", "static", "struct", "switch", "this", "throw", "true", "try",
        "typeof", "unchecked", "unsafe", "using", "virtual", "volatile", "while",
    ];
    KEYWORDS.contains(&word)
}

/// Splits a declarator section on its TOP-LEVEL commas (depth-0, outside string/char literals),
/// honoring `()`, `[]`, and `{}` nesting. To avoid splitting a generic argument list at the
/// wrong comma (e.g. `new Dictionary<int, string>()`, whose comma is at depth 0 to a tracker
/// that does not balance `<`/`>`), a section that contains any `<` is left UNSPLIT -- conservative,
/// so such multi-declarator forms collapse to a single declarator rather than misparse.
fn split_top_level_commas(section: &str) -> Vec<&str> {
    if section.contains('<') {
        return vec![section];
    }
    let mut parts = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    let mut scan = LiteralScan::new();
    for (index, ch) in section.char_indices() {
        if scan.step(ch) {
            continue;
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&section[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&section[start..]);
    parts
}

/// Splits one declarator into `(name, Some(initializer))` or `(name, None)` for a bare
/// declarator. The name is the leading identifier; the initializer is whatever follows the
/// declarator's top-level `=` (an assignment `=`, not `==`/`<=`/`>=`/`!=`/`=>`). Returns `None`
/// if the declarator does not start with a single identifier (so it is not a real declarator).
fn split_declarator(declarator: &str) -> Option<(&str, Option<&str>)> {
    let declarator = declarator.trim();
    let bytes = declarator.as_bytes();
    if bytes.is_empty() || bytes[0].is_ascii_digit() {
        return None;
    }
    let mut end = 0;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    if end == 0 {
        return None;
    }
    let name = &declarator[..end];
    let rest = declarator[end..].trim_start();
    if rest.is_empty() {
        return Some((name, None));
    }
    let assignment = find_assignment_eq(rest)?;
    if assignment != 0 {
        return None;
    }
    Some((name, Some(rest[1..].trim())))
}

/// The byte offset in `text` of the first top-level assignment `=`: a single `=` that is not
/// part of `==`, `<=`, `>=`, `!=`, or `=>`, found outside string/char literals and outside any
/// `()`/`[]`/`{}` nesting. Returns `None` if there is none.
fn find_assignment_eq(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth: i32 = 0;
    let mut scan = LiteralScan::new();
    for (index, ch) in text.char_indices() {
        if scan.step(ch) {
            continue;
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '=' if depth == 0 => {
                let prev = index.checked_sub(1).map(|p| bytes[p]);
                let next = bytes.get(index + 1).copied();
                let part_of_two = matches!(prev, Some(b'!' | b'<' | b'>' | b'='))
                    || matches!(next, Some(b'=' | b'>'));
                if !part_of_two {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// A tiny left-to-right scanner that tracks whether the current character sits inside a `"..."`
/// string or `'...'` char literal, so structural scanners (bracket balance, comma/`=` finding)
/// can ignore brackets, commas, and `=` that are really literal text. Backslash escapes inside
/// either literal are honored. Verbatim (`@"..."`) and interpolated (`$"..."`) strings are NOT
/// specially handled -- a known limit of the surface heuristic; their `\"`-less or brace content
/// can fool the scanner, which is acceptable for one-liner REPL classification.
struct LiteralScan {
    state: ScanState,
}

#[derive(PartialEq)]
enum ScanState {
    Normal,
    String,
    StringEscape,
    Char,
    CharEscape,
}

impl LiteralScan {
    fn new() -> LiteralScan {
        LiteralScan {
            state: ScanState::Normal,
        }
    }

    /// Advances over one character; returns `true` if it is part of a string/char literal
    /// (including the opening and closing delimiters), i.e. callers should NOT treat it as
    /// structural. Returns `false` only for characters in `Normal` state outside any literal.
    fn step(&mut self, ch: char) -> bool {
        match self.state {
            ScanState::Normal => match ch {
                '"' => {
                    self.state = ScanState::String;
                    true
                }
                '\'' => {
                    self.state = ScanState::Char;
                    true
                }
                _ => false,
            },
            ScanState::String => {
                self.state = match ch {
                    '\\' => ScanState::StringEscape,
                    '"' => ScanState::Normal,
                    _ => ScanState::String,
                };
                true
            }
            ScanState::StringEscape => {
                self.state = ScanState::String;
                true
            }
            ScanState::Char => {
                self.state = match ch {
                    '\\' => ScanState::CharEscape,
                    '\'' => ScanState::Normal,
                    _ => ScanState::Char,
                };
                true
            }
            ScanState::CharEscape => {
                self.state = ScanState::Char;
                true
            }
        }
    }

    /// Whether the scan ended in the middle of a literal (an unterminated `"` or `'`), which
    /// makes an accumulated submission incomplete.
    fn in_literal(&self) -> bool {
        self.state != ScanState::Normal
    }
}

/// Whether an accumulated multi-line submission looks COMPLETE and ready to run, used by the
/// interactive `--session` loop to decide between submitting and showing a continuation prompt.
/// This is a surface heuristic (the real arbiter is csc), tuned so the common cases never strand
/// the user: a blank line in the loop force-submits regardless of what this returns.
///
/// `text` is complete when, ignoring string/char literals:
/// - every `()`, `[]`, `{}` is balanced (and none closed before it opened -- an over-closed line
///   is treated as complete so its csc error surfaces rather than trapping the user), and the
///   scan did not end inside a literal; and
/// - either the trimmed text ends with `;` or `}` (a finished statement or block), or it is a
///   bare expression -- one that neither begins with a body-requiring keyword (`if`, `for`,
///   `while`, `foreach`, `do`, `else`, `switch`, `using`, `lock`, `fixed`, `try`, `catch`,
///   `finally`) nor ends with a dangling operator/separator (`= + - * / % & | ^ < > , . ? :`,
///   or a trailing `&&` / `||` / `=>`).
///
/// Everything else (open brackets, mid-literal, a dangling operator, or an unterminated control
/// statement) is INCOMPLETE, so the caller keeps reading.
#[must_use]
pub fn submission_is_complete(text: &str) -> bool {
    let mut depth: i32 = 0;
    let mut over_closed = false;
    let mut scan = LiteralScan::new();
    for ch in text.chars() {
        if scan.step(ch) {
            continue;
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth < 0 {
                    over_closed = true;
                }
            }
            _ => {}
        }
    }
    if over_closed {
        return true;
    }
    if depth > 0 || scan.in_literal() {
        return false;
    }

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.ends_with(';') || trimmed.ends_with('}') {
        return true;
    }
    !begins_with_body_keyword(trimmed) && !ends_with_dangling_operator(trimmed)
}

/// Whether `text`'s first identifier token is a statement keyword that REQUIRES a following body
/// or terminator, so a balanced-but-bodyless `if (c)` reads as incomplete rather than complete.
fn begins_with_body_keyword(text: &str) -> bool {
    let head: String = text
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    matches!(
        head.as_str(),
        "if" | "for"
            | "foreach"
            | "while"
            | "do"
            | "else"
            | "switch"
            | "using"
            | "lock"
            | "fixed"
            | "try"
            | "catch"
            | "finally"
    )
}

/// Whether `text` ends with an operator/separator that cannot terminate an expression, so more
/// input must follow (`1 +`, `int x =`, `a &&`, `obj.`, `cond ?`).
fn ends_with_dangling_operator(text: &str) -> bool {
    let text = text.trim_end();
    if text.ends_with("&&") || text.ends_with("||") || text.ends_with("=>") {
        return true;
    }
    matches!(
        text.chars().next_back(),
        Some('=' | '+' | '-' | '*' | '/' | '%' | '&' | '|' | '^' | '<' | '>' | ',' | '.' | '?' | ':')
    )
}

/// Resolves a method by its loader-recorded qualified name (e.g. `__Repl.SetX`) to its
/// [`MethodId`], scanning the module's methods. Names come from the metadata the loader
/// recorded, so this is resolution by declared identity -- no token-row arithmetic.
fn find_method(module: &Module, name: &str) -> Option<MethodId> {
    let mut id: MethodId = 0;
    while module.method(id).is_some() {
        if module.method_name(id) == Some(name) {
            return Some(id);
        }
        id += 1;
    }
    None
}

/// The persistent instance's current handle, read back from its static root `slot` (a
/// collection may have relocated it since it was stored). Errors if the slot no longer
/// holds an object reference -- which would mean the root was lost, a bug.
fn current_instance(vm: &Vm, slot: usize) -> Result<ObjectRef, String> {
    match vm.static_field(slot) {
        Some(Value::Object(reference)) => Ok(reference),
        other => Err(format!(
            "persistent __Repl instance root was lost (slot held {other:?})"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_stateless_expressions() {
        if Toolchain::discover().is_err() {
            eprintln!("skipping: csc/ref-pack not found on this host");
            return;
        }
        assert_eq!(eval("1 + 2 * 3").as_deref(), Ok("7\n"));
        assert_eq!(eval("\"hello\".Length").as_deref(), Ok("5\n"));
        assert_eq!(eval("System.Math.Max(3, 7)").as_deref(), Ok("7\n"));
    }

    #[test]
    fn reports_compile_errors_as_err() {
        if Toolchain::discover().is_err() {
            eprintln!("skipping: csc/ref-pack not found on this host");
            return;
        }
        assert!(eval("nonexistent + 1").is_err());
    }

    /// Unwraps a [`Submission::Declaration`] into its `(type, name)` pairs and the `__Submit`
    /// body, or panics with `message` -- a test helper so the classifier asserts read cleanly.
    fn expect_declaration(submission: Submission, message: &str) -> (Vec<(String, String)>, String) {
        match submission {
            Submission::Declaration { decls, body } => (
                decls
                    .into_iter()
                    .map(|field| (field.type_text, field.name))
                    .collect(),
                body,
            ),
            _ => panic!("{message}"),
        }
    }

    #[test]
    fn classifies_declarations_statements_and_expressions() {
        let (decls, body) = expect_declaration(
            classify("int x = 5;"),
            "`int x = 5;` should classify as a declaration",
        );
        assert_eq!(decls, vec![("int".to_owned(), "x".to_owned())]);
        assert_eq!(body, "x = 5;");

        assert!(matches!(
            classify("System.Int32 n = 1;"),
            Submission::Declaration { .. }
        ));
        assert!(matches!(
            classify("List<int> xs = new List<int>();"),
            Submission::Declaration { .. }
        ));
        assert!(matches!(
            classify("int[] a = new int[3];"),
            Submission::Declaration { .. }
        ));
        match classify("System.Console.WriteLine(x);") {
            Submission::Statement { body } => assert_eq!(body, "System.Console.WriteLine(x);"),
            _ => panic!("a call statement should classify as a statement"),
        }
        match classify("x * 2") {
            Submission::Expression { body } => {
                assert_eq!(body, "System.Console.WriteLine(x * 2);");
            }
            _ => panic!("a bare expression should classify as an expression"),
        }
        assert!(matches!(classify("x == 5"), Submission::Expression { .. }));
        assert!(matches!(classify("x = 5;"), Submission::Statement { .. }));
    }

    #[test]
    fn classifies_multi_variable_declarations() {
        let (decls, body) = expect_declaration(
            classify("int a = 1, b = 2;"),
            "`int a = 1, b = 2;` should classify as a declaration",
        );
        assert_eq!(
            decls,
            vec![
                ("int".to_owned(), "a".to_owned()),
                ("int".to_owned(), "b".to_owned()),
            ]
        );
        assert_eq!(body, "a = 1; b = 2;");

        let (decls, body) = expect_declaration(
            classify("int p = 3, q;"),
            "`int p = 3, q;` should classify as a declaration",
        );
        assert_eq!(
            decls,
            vec![
                ("int".to_owned(), "p".to_owned()),
                ("int".to_owned(), "q".to_owned()),
            ]
        );
        assert_eq!(body, "p = 3;");

        let (decls, body) = expect_declaration(
            classify("int m = System.Math.Max(1, 2);"),
            "a single declarator with a comma in its initializer is one field",
        );
        assert_eq!(decls, vec![("int".to_owned(), "m".to_owned())]);
        assert_eq!(body, "m = System.Math.Max(1, 2);");
    }

    #[test]
    fn does_not_misclassify_statements_or_inferred_types() {
        assert!(matches!(classify("return x;"), Submission::Statement { .. }));
        assert!(matches!(classify("throw e;"), Submission::Statement { .. }));
        assert!(matches!(
            classify("using System.Text;"),
            Submission::Statement { .. }
        ));
        assert!(matches!(classify("var z = 1;"), Submission::Statement { .. }));
        assert!(matches!(
            classify("dynamic d = 1;"),
            Submission::Statement { .. }
        ));
    }

    #[test]
    fn submission_completeness_predicate() {
        assert!(submission_is_complete("int x = 5;"));
        assert!(submission_is_complete("if (x > 0) { y = 1; }"));
        assert!(submission_is_complete("new[] { 1, 2, 3 }"));
        assert!(submission_is_complete("1 + 2"));
        assert!(submission_is_complete("x * 2"));
        assert!(!submission_is_complete("if (x > 0) {"));
        assert!(!submission_is_complete("System.Math.Max(1,"));
        assert!(!submission_is_complete("new[] { 1, 2"));
        assert!(!submission_is_complete("int x ="));
        assert!(!submission_is_complete("1 +"));
        assert!(!submission_is_complete("a &&"));
        assert!(!submission_is_complete("if (c)"));
        assert!(!submission_is_complete("while (true)"));
        assert!(submission_is_complete("Console.WriteLine(\"a)b\");"));
        assert!(submission_is_complete("char c = ')';"));
        assert!(!submission_is_complete("string s = \"oops"));
        assert!(submission_is_complete("y = 1; }"));
    }

    #[test]
    fn stateful_session_persists_declarations() {
        let mut session = match ReplSession::new() {
            Ok(session) => session,
            Err(_) => {
                eprintln!("skipping: csc/ref-pack not found on this host");
                return;
            }
        };
        assert_eq!(session.submit("int x = 5;").as_deref(), Ok(""));
        assert_eq!(session.submit("x * 2").as_deref(), Ok("10\n"));
        assert_eq!(session.submit("int y = x + 3;").as_deref(), Ok(""));
        assert_eq!(session.submit("y").as_deref(), Ok("8\n"));
    }

    #[test]
    fn redefinition_replaces_the_existing_field() {
        let mut session = match ReplSession::new() {
            Ok(session) => session,
            Err(_) => {
                eprintln!("skipping: csc/ref-pack not found on this host");
                return;
            }
        };
        assert_eq!(session.submit("int x = 5;").as_deref(), Ok(""));
        assert_eq!(session.submit("int x = 9;").as_deref(), Ok(""));
        assert_eq!(session.submit("x").as_deref(), Ok("9\n"));
        assert_eq!(session.fields.len(), 1);
        assert_eq!(session.fields[0].name, "x");
    }

    #[test]
    fn redefinition_with_a_new_type_resets_the_slot() {
        let mut session = match ReplSession::new() {
            Ok(session) => session,
            Err(_) => {
                eprintln!("skipping: csc/ref-pack not found on this host");
                return;
            }
        };
        assert_eq!(session.submit("int v = 7;").as_deref(), Ok(""));
        assert_eq!(session.submit("long v;").as_deref(), Ok(""));
        assert_eq!(session.submit("v").as_deref(), Ok("0\n"));
        assert_eq!(session.fields.len(), 1);
        assert_eq!(session.fields[0].type_text, "long");
    }

    #[test]
    fn multi_variable_declaration_persists_both_fields() {
        let mut session = match ReplSession::new() {
            Ok(session) => session,
            Err(_) => {
                eprintln!("skipping: csc/ref-pack not found on this host");
                return;
            }
        };
        assert_eq!(session.submit("int a = 1, b = 2;").as_deref(), Ok(""));
        assert_eq!(session.fields.len(), 2);
        assert_eq!(session.submit("a + b").as_deref(), Ok("3\n"));
    }

    /// The emitted `__Repl` class is a `/target:library` class with NO entry point: the dummy
    /// `Main` workaround is gone now that the loader has [`load_library`]. This pins the emit's
    /// output directly -- it needs no csc, so it never skips -- proving the source carries only
    /// the accumulated fields and `__Submit`, and no `Main`.
    #[test]
    fn emitted_class_has_no_main_entry_point() {
        let session = ReplSession {
            tools: Toolchain {
                dotnet: PathBuf::new(),
                csc: PathBuf::new(),
                references: Vec::new(),
            },
            fields: vec![
                ReplField {
                    type_text: "int".to_owned(),
                    name: "x".to_owned(),
                },
                ReplField {
                    type_text: "string".to_owned(),
                    name: "s".to_owned(),
                },
            ],
            counter: 0,
            state: None,
        };

        let source = session.emit_class("System.Console.WriteLine(x);");
        assert!(
            !source.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|word| word == "Main"),
            "emitted __Repl must carry no Main; got:\n{source}"
        );
        assert!(source.contains("public int x;"), "field x missing:\n{source}");
        assert!(source.contains("public string s;"), "field s missing:\n{source}");
        assert!(source.contains("public void __Submit()"), "__Submit missing:\n{source}");
        assert!(
            source.contains("System.Console.WriteLine(x);"),
            "submission body missing:\n{source}"
        );
    }
}
