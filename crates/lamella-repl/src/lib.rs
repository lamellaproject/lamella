//! A host-PC C# REPL on the lamella interpreter, bootstrap-compiled with csc.

use lamella_load::{DeltaContext, load, load_bootstrap, load_delta, load_library};
use lamella_metadata::Assembly;
use lamella_ves::intrinsics::object_to_string;
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

#[derive(Clone, Copy)]
enum CompileTarget {
    Exe,
    Library,
}

impl CompileTarget {
    fn switch(self) -> &'static str {
        match self {
            CompileTarget::Exe => "/target:exe",
            CompileTarget::Library => "/target:library",
        }
    }
}

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

struct TempProgram {
    source: PathBuf,
    assembly: PathBuf,
}

impl TempProgram {
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

    fn cleanup(&self) {
        let _ = fs::remove_file(&self.source);
        let _ = fs::remove_file(&self.assembly);
    }
}

/// A persistent-state REPL session: ONE interpreter, ONE heap, and ONE `__Repl`
/// instance kept alive across submissions, so declared state survives line to line.
///
/// This is the runtime half of REPL increment 2 (the persistent-state model). Where
/// [`eval`] runs each submission against a fresh [`Vm`] -- so nothing carries over --
/// a `Session` holds a single [`Vm`] (heap + statics) and a single `__Repl` instance,
/// and runs each submission as an *instance method of that one object*. A field the
/// first submission writes is still set when the next submission reads it, because the
/// instance (and the heap it lives on) is reused rather than rebuilt.
pub struct Session {
    vm: Vm,
    module: Module,
    instance: ObjectRef,
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

/// The qualified name of the persistent `__Repl`'s parameterless constructor, in the reserved
/// `<repl>` namespace the incremental model uses (the loader records a method's name as
/// `namespace.type.method`). Anchors both the type to instantiate and the ctor to run.
const REPL_CTOR_NAME: &str = "<repl>.__Repl..ctor";

/// An INCREMENTAL-load REPL session: ONE interpreter, ONE heap, and ONE `<repl>.__Repl`
/// instance that GROWS as submissions are loaded -- the successor to [`Session`]'s
/// re-emit+migrate that lets REFERENCE-typed state (a `string`/array/object) survive across
/// submissions.
///
/// This is the runtime half of the incremental REPL model (`docs/repl-incremental-model.md`),
/// prototyped against hand-authored IL deltas while the compiler builds the matching emit. The
/// session loads an empty `__Repl` bootstrap ONCE and creates one instance of it; each
/// [`IncrementalSession::submit`] loads a SEPARATE delta assembly that references `__Repl` and
/// its prior fields by name (a TypeRef + FieldRefs, no `__Repl` TypeDef) and carries one
/// `Submit$N(__Repl s)` method. A FieldRef the loader cannot resolve to an existing `__Repl`
/// field is a NEW field: it grows the type and the single live instance in place, on the SAME
/// heap. Running `Submit$N` against that one instance is what persists state -- and because the
/// heap is never rebuilt, a reference field's `ObjectRef` set by one submission still points at
/// a live object when a later submission reads it.
///
pub struct IncrementalSession {
    vm: Vm,
    module: Module,
    context: DeltaContext,
    instance: ObjectRef,
    root_slot: usize,
    compiler: Option<lamella_assemble::Session>,
}

impl IncrementalSession {
    /// Opens an incremental session over the empty `<repl>.__Repl` in the bootstrap assembly at
    /// `path`: loads it, allocates the single `__Repl` instance, runs its parameterless `.ctor`,
    /// roots it for the collector, and prepares the delta context the submissions grow.
    ///
    /// The bootstrap must define `class <repl>.__Repl` with a parameterless instance `.ctor` and
    /// no fields (it grows one per declaration as deltas load). It needs no entry point;
    /// [`load_library`] binds it without one.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read, is not a valid assembly, fails to load, declares
    /// no `<repl>.__Repl..ctor`, or the constructor traps.
    pub fn open(path: &Path) -> Result<IncrementalSession, String> {
        let bytes =
            fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::open_from_bytes(&bytes, None)
    }

    /// Opens an incremental session that drives the COMPILER: it creates a
    /// [`lamella_assemble::Session`] over `references` (the BCL -- `&[]` suffices for an
    /// int-only stream), emits that session's one-time bootstrap (an empty `<repl>.__Repl`),
    /// and loads it exactly as [`IncrementalSession::open`] loads a hand-authored bootstrap.
    /// [`IncrementalSession::submit_source`] then compiles each C# line through the session
    /// into a delta image and loads it. This is the host incremental REPL: real C# in, the
    /// growing persistent `__Repl` instance behind it.
    ///
    /// # Errors
    /// Returns `Err` if the compiler's bootstrap cannot be emitted, is not a valid assembly,
    /// fails to load, declares no `<repl>.__Repl..ctor`, or the constructor traps.
    pub fn open_compiler(references: &[Assembly]) -> Result<IncrementalSession, String> {
        let compiler = lamella_assemble::Session::new(references);
        let bootstrap = compiler
            .bootstrap()
            .map_err(|error| format!("cannot emit bootstrap: {error:?}"))?;
        Self::open_from_bytes(&bootstrap, Some(compiler))
    }

    fn open_from_bytes(
        bytes: &[u8],
        compiler: Option<lamella_assemble::Session>,
    ) -> Result<IncrementalSession, String> {
        let assembly =
            Assembly::read(bytes).map_err(|error| format!("cannot read metadata: {error:?}"))?;
        let (module, name_index, type_index) = load_bootstrap(&assembly);

        let ctor = find_method(&module, REPL_CTOR_NAME)
            .ok_or_else(|| format!("bootstrap defines no {REPL_CTOR_NAME}"))?;
        let type_id = module
            .method_type(ctor)
            .ok_or_else(|| format!("{REPL_CTOR_NAME} has no declaring type"))?;
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
            .map_err(|trap| format!("trap running {REPL_CTOR_NAME}: {trap}"))?;
        let instance = current_instance(&vm, root_slot)?;

        Ok(IncrementalSession {
            vm,
            module,
            context: DeltaContext::new(type_id, name_index, type_index),
            instance,
            root_slot,
            compiler,
        })
    }

    /// Submits one delta assembly at `path`: loads it into the persistent module, grows the
    /// single `__Repl` instance for any new fields the delta introduces, runs its `Submit$N`
    /// against that instance, and returns the displayed result.
    ///
    /// A statement submission returns void -> `""`. An expression submission returns its value
    /// (boxed to `object` by the delta); this renders it the same way `Object.ToString` would
    /// (`"10"`, `"hi"`), with no trailing newline -- the value display, not console output.
    ///
    /// # Errors
    /// Returns `Err` if the delta cannot be read / parsed / bound, the instance cannot be grown,
    /// or `Submit$N` traps.
    pub fn submit(&mut self, path: &Path) -> Result<String, String> {
        let bytes =
            fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        self.submit_delta_bytes(&bytes, &path.display().to_string())
    }

    /// Compiles one real C# REPL line through the driving [`lamella_assemble::Session`] and, when
    /// it compiles cleanly, loads the emitted delta into the persistent module, grows the single
    /// `__Repl` instance for any new session variable, runs the submission against it, and returns
    /// the displayed result. The session is only available when opened with
    /// [`IncrementalSession::open_compiler`].
    ///
    /// A statement compiles to a void `Submit$N` and displays `""` (expression-return -- a boxed
    /// `object` the compiler returns -- is the compiler's following increment; until then read a
    /// persisted variable with [`IncrementalSession::instance_field`] rather than expecting the
    /// value back). On a clean compile the submission's new variables are committed in BOTH halves:
    /// the compiler advances its shape and the loader grows the instance, so a later line sees them.
    ///
    /// # Errors
    /// Returns `Err` carrying the compiler diagnostics when the line does not compile (a diagnostic
    /// error or a not-yet-lowered construct -> no delta); in that case neither half advances, so a
    /// corrected retry is not skewed. Also `Err` if the emitted delta cannot be loaded, the instance
    /// cannot be grown, or `Submit$N` traps. Errors with a clear message if the session has no
    /// compiler (it was opened with [`IncrementalSession::open`] for hand-authored deltas).
    pub fn submit_source(&mut self, src: &str) -> Result<String, String> {
        let compiler = self
            .compiler
            .as_mut()
            .ok_or("this IncrementalSession was not opened with a compiler (use open_compiler)")?;
        let result = compiler.compile_submission(src);
        let Some(delta) = result.delta else {
            return Err(format_diagnostics(&result.diagnostics, result.emit_error));
        };
        self.submit_delta_bytes(&delta, "submission")
    }

    fn submit_delta_bytes(&mut self, bytes: &[u8], label: &str) -> Result<String, String> {
        let delta =
            Assembly::read(bytes).map_err(|error| format!("cannot read metadata: {error:?}"))?;
        let info = load_delta(&mut self.module, &mut self.context, &delta)
            .map_err(|error| format!("cannot load delta {label}: {error}"))?;

        if !info.new_field_defaults.is_empty() {
            self.vm
                .heap_mut()
                .grow_instance(self.instance, &info.new_field_defaults)
                .ok_or_else(|| "persistent __Repl instance could not be grown".to_owned())?;
        }

        let result = run(
            &self.module,
            &mut self.vm,
            info.submit,
            vec![Value::Object(self.instance)],
        )
        .map_err(|trap| format!("trap running submission: {trap}"))?;
        self.instance = current_instance(&self.vm, self.root_slot)?;

        Ok(self.display(result))
    }

    /// Renders a submission's return value for display: void (`None`) as `""`, otherwise exactly
    /// as `Object.ToString` would (a boxed value by its representation, a string verbatim),
    /// reusing the runtime's own `object_to_string` so the display matches the interpreter.
    fn display(&mut self, result: Option<Value>) -> String {
        let Some(value) = result else {
            return String::new();
        };
        let rendered = object_to_string(&mut self.vm, &self.module, &[value]);
        if let Ok(instance) = current_instance(&self.vm, self.root_slot) {
            self.instance = instance;
        }
        match rendered {
            Ok(Some(Value::Object(reference))) => self
                .vm
                .heap()
                .as_string(reference)
                .map(|chars| String::from_utf16_lossy(&chars))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// The value of instance field `slot` of the persistent `__Repl`, for tests/inspection.
    #[must_use]
    pub fn instance_field(&self, slot: u32) -> Option<Value> {
        self.vm.heap().instance_field(self.instance, slot)
    }

    /// How many fields the persistent `__Repl` has grown to (the count of declarations that
    /// added a field), for tests/inspection.
    #[must_use]
    pub fn field_count(&self) -> usize {
        self.context.field_count()
    }

    /// The assembly id the NEXT submitted delta will load under (one past the last submission;
    /// the first delta is asm 1, the bootstrap is asm 0). For tests/inspection: a value `>= 4`
    /// after several submissions confirms each delta took a distinct slot and >2 assemblies are
    /// resolving at once, past the old single-bit `asm_key` cap.
    #[must_use]
    pub fn next_delta_asm(&self) -> u8 {
        self.context.next_delta_asm()
    }
}

#[derive(Clone)]
struct ReplField {
    type_text: String,
    name: String,
}

enum Submission {
    Declaration {
        decls: Vec<ReplField>,
        body: String,
    },
    Statement { body: String },
    Expression { body: String },
}

/// A stateful host-PC C# REPL built on the csc bootstrap: declarations persist across
/// submissions, so `int x = 5;` then `x * 2` prints `10`.
///
/// ```no_run
/// let mut session = lamella_repl::ReplSession::new().unwrap();
/// assert_eq!(session.submit("int x = 5;").unwrap(), "");
/// assert_eq!(session.submit("x * 2").unwrap(), "10\n");
/// ```
pub struct ReplSession {
    tools: Toolchain,
    fields: Vec<ReplField>,
    counter: u64,
    state: Option<ReplState>,
}

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
    fn body(&self) -> &str {
        match self {
            Submission::Declaration { body, .. }
            | Submission::Statement { body }
            | Submission::Expression { body } => body,
        }
    }
}

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

fn format_diagnostics(
    diagnostics: &[lamella_assemble::Diagnostic],
    emit_error: Option<lamella_assemble::EmitError>,
) -> String {
    if diagnostics.is_empty() {
        return match emit_error {
            Some(lamella_assemble::EmitError::Unsupported(reason)) => {
                format!("submission not lowered yet: {reason}")
            }
            None => "submission produced no delta and no diagnostics".to_owned(),
        };
    }
    let mut rendered = String::new();
    for diagnostic in diagnostics {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        rendered.push_str(&format!("CS{:04}: {}", diagnostic.code, diagnostic.message));
    }
    rendered
}

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
