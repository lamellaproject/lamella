//! The interpreter debug backend: drives a [`lamella_cil_runtime::Session`] behind the
//! [`DebugBackend`] seam, so the adapter debugs interpreted code through the same
//! interface an on-device target uses.

use lamella_cil::{Instruction, Operand};
use std::borrow::Cow;
use lamella_debug_backend::{
    DebugBackend, Disassembled, Frame, Register, Scope, SourceLocation, Stop, ThreadInfo, Variable,
};
use lamella_metadata::{Assembly, PortablePdb, SigType, StackClass};
use lamella_token::Token;
use lamella_cil_runtime::{MethodId, Module, Session, Slice, Threads, Value, Vm, Watch, module::TypeId};
use std::collections::{BTreeMap, BTreeSet};

/// The `MethodDef` metadata table tag, for rebuilding a method token to map a PDB
/// `method_rid` to the interpreter's `MethodId`.
const METHOD_DEF: u8 = 0x06;

/// How long one slice of the program runs, in the machine's milliseconds, before the backend
/// answers: long enough that a polling host's own wait between polls costs the program little of its
/// time, short enough that a pause is answered at once.
const SLICE_MILLIS: u64 = 50;

/// The most instructions one slice runs, which is how a slice ends on a machine with no clock.
const SLICE_BUDGET: u32 = 1 << 20;

/// What .NET's debugger names a program's main thread.
const MAIN_THREAD_NAME: &str = "Main Thread";

/// What .NET's debugger names a thread its program has not named.
const UNNAMED_THREAD_NAME: &str = "<No Name>";

/// What the program is doing for the debugger between the backend's calls.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// Nothing: the program is stopped, or has ended.
    Nothing,
    /// Running until a thread reaches a breakpoint, or the program ends.
    Resume,
    /// Running until this thread (the scheduler's id) has run one instruction: a step.
    Step(u32),
}

/// What one slice of the program came to ([`InterpreterBackend::run_slice`]).
enum Outcome {
    /// A stop to report.
    Stopped(Stop),
    /// The program runs on.
    Running,
    /// The program runs on, and only a pause or its end will change that: no thread can run and
    /// nothing will wake one.
    Waiting,
}

/// A [`DebugBackend`] over the interpreter: it owns the module being debugged, the
/// runtime context, and the program's threads once launched.
///
/// The program's threads run as they do without the debugger, under the interpreter's scheduler: a
/// started thread runs, a breakpoint stops the program on whichever thread reaches it, and a thread
/// asleep for good blocks only itself. A step moves one thread while the others run, as on .NET.
///
/// By default the backend runs the program inside each call that resumes it, until it stops, for a
/// host that never polls -- a page answering one request at a time. Such a host is told the program
/// is running only when nothing but a pause can change it. A host that polls between requests makes
/// the backend [`polled`](InterpreterBackend::polled), so a pause reaches a program that never
/// stops.
pub struct InterpreterBackend {
    module: Module,
    entry: u32,
    vm: Vm,
    /// The program's threads, once launched.
    threads: Option<Threads>,
    /// Instruction breakpoints as `(method, instruction)`, kept so they survive the
    /// `launch` that (re)creates the threads.
    breakpoints: Vec<(u32, u32)>,
    /// UTF-16 code units of console output already drained by [`Self::take_output`].
    output_sent: usize,
    /// UTF-16 code units of debug-channel output already drained by [`Self::take_debug_output`].
    debug_sent: usize,
    /// The standalone Portable PDB bytes, when source mapping is available.
    pdb: Option<Vec<u8>>,
    /// `MethodId` -> `MethodDef` rid: the reverse of the module's token binding, so a
    /// stack frame's method maps back to its PDB row for `source_location`.
    method_rid: BTreeMap<MethodId, u32>,
    /// Per method, the CIL byte offsets carrying a (non-hidden) sequence point: the
    /// statement boundaries a source-level step may land on. Empty without a PDB.
    seq_boundaries: BTreeMap<MethodId, BTreeSet<u32>>,
    /// The thread (the scheduler's id) stopped on a breakpoint already reported to the client, which
    /// steps off it when the program goes on. `None` at launch -- so a breakpoint on the entry
    /// instruction fires rather than being stepped over.
    reported: Option<u32>,
    /// The thread (the scheduler's id) the calls that read or move one thread act on
    /// ([`DebugBackend::selected_thread`]): at every stop, the one the stop happened on.
    selected: u32,
    /// Whether the host polls this backend between requests ([`InterpreterBackend::polled`]).
    polled: bool,
    /// What the program is doing between the backend's calls.
    pending: Pending,
    /// The program's exit code: the entry method's `int` return (else 0), captured when the
    /// session reports `Status::Done`. Reported in the adapter's `exited` event, so a debug run
    /// surfaces the real exit code (not a hardcoded 0).
    exit_code: i32,
    /// The code a run that ended in a fault ends with, captured when it faults: the trap's own code
    /// (134 for an exception the program did not catch, 1 for a trap of the interpreter's own).
    /// `None` until a run faults.
    fault_exit_code: Option<i32>,
    /// Per method, the [`LocalZero`] of each local by slot, from the program's local signatures
    /// ([`InterpreterBackend::with_local_types`]): `None` for a type whose zero neither its
    /// signature nor the module decides. Empty until the backend is given the program.
    local_zeros: BTreeMap<MethodId, Vec<Option<LocalZero>>>,
}

/// What a local holds before its method first stores it: every local starts at its type's zero, a
/// reference at null and a value type zeroed (ECMA-335 6th ed I.12.3.2 and II.15.4.1.3).
#[derive(Clone, Copy)]
enum LocalZero {
    /// The zero of a stack class: an `int`'s 0, a `long`'s, a reference's null.
    Class(StackClass),
    /// The struct of this type with every field at its zero.
    Struct(TypeId),
}

/// The process exit code an entry method's return maps to: its `int` return, or 0 for a `void` (or
/// non-`int`) entry -- matching the non-debug run path's convention (`run_bytes`).
fn exit_code_of(returned: &Option<Value>) -> i32 {
    match returned {
        Some(Value::Int32(code)) => *code,
        _ => 0,
    }
}

impl InterpreterBackend {
    /// Creates a backend that owns `module`, entered at `entry`.
    ///
    /// Its program has no clock until [`InterpreterBackend::configure_machine`] gives it one.
    #[must_use]
    pub fn new(module: Module, entry: u32) -> InterpreterBackend {
        InterpreterBackend {
            module,
            entry,
            vm: Vm::new(),
            threads: None,
            breakpoints: Vec::new(),
            output_sent: 0,
            debug_sent: 0,
            pdb: None,
            method_rid: BTreeMap::new(),
            seq_boundaries: BTreeMap::new(),
            reported: None,
            selected: 0,
            polled: false,
            pending: Pending::Nothing,
            exit_code: 0,
            fault_exit_code: None,
            local_zeros: BTreeMap::new(),
        }
    }

    /// Creates a backend with source mapping from a standalone Portable PDB
    /// (`pdb_bytes`), building the `MethodId` -> `method_rid` reverse map from the
    /// module's token binding so frames and breakpoints map to and from source.
    ///
    /// Bytes that are not a Portable PDB map nothing, so the backend has no source and is debugged
    /// by CIL instruction, as one made by [`InterpreterBackend::new`] is. A caller that must not
    /// serve a session without its source reads the PDB first and refuses it there.
    ///
    /// Its program has no clock until [`InterpreterBackend::configure_machine`] gives it one.
    #[must_use]
    pub fn with_pdb(module: Module, entry: u32, pdb_bytes: Vec<u8>) -> InterpreterBackend {
        let mut method_rid = BTreeMap::new();
        let mut seq_boundaries = BTreeMap::new();
        let read = PortablePdb::read(&pdb_bytes);
        let mapped = read.is_ok();
        if let Ok(pdb) = read {
            let program_asm = module.method_asm(entry);
            for rid in 1..=pdb.method_count() {
                if let Some(id) = module.resolve(program_asm, Token::new(METHOD_DEF, rid)) {
                    method_rid.insert(id, rid);
                    let offsets: BTreeSet<u32> = pdb
                        .sequence_points(rid)
                        .iter()
                        .filter(|point| !point.is_hidden)
                        .map(|point| point.il_offset)
                        .collect();
                    if !offsets.is_empty() {
                        seq_boundaries.insert(id, offsets);
                    }
                }
            }
        }
        InterpreterBackend {
            module,
            entry,
            vm: Vm::new(),
            threads: None,
            breakpoints: Vec::new(),
            output_sent: 0,
            debug_sent: 0,
            pdb: mapped.then_some(pdb_bytes),
            method_rid,
            seq_boundaries,
            reported: None,
            selected: 0,
            polled: false,
            pending: Pending::Nothing,
            exit_code: 0,
            fault_exit_code: None,
            local_zeros: BTreeMap::new(),
        }
    }

    /// This backend with the zero of each local of `program`'s methods, from their local
    /// signatures, so a frame's variables list each local its PDB names, stored or not: one the
    /// method has not stored yet at its type's zero, the value every local starts at -- a struct as
    /// that struct with its fields at zero -- or as not assigned yet where neither its signature nor
    /// the module decides that zero ([`local_type_zero`]).
    ///
    /// `program` is the assembly the backend's module was loaded from, whose portable PDB
    /// [`InterpreterBackend::with_pdb`] was given. A backend without that PDB names no local, and
    /// gains nothing from this.
    #[must_use]
    pub fn with_local_types(mut self, program: &Assembly<'_>) -> InterpreterBackend {
        let ids: BTreeMap<u32, MethodId> = self.method_rid.iter().map(|(&id, &rid)| (rid, id)).collect();
        let asm = self.module.method_asm(self.entry);
        for type_def in program.type_defs() {
            for method in type_def.methods() {
                if let Some(&id) = ids.get(&method.rid()) {
                    let zeros = method.local_variables().iter().map(|local| local_type_zero(&self.module, asm, local)).collect();
                    self.local_zeros.insert(id, zeros);
                }
            }
        }
        self
    }

    /// This backend with its program's machine set up by `configure`, which is handed the module
    /// and the machine before the program starts. It is where the program's clock comes from.
    ///
    /// The interpreter keeps no clock of its own, so a program given none reads the year 1 from
    /// `DateTime.Now` and 0 from `Environment.TickCount`, and its `Thread.Sleep` returns at once.
    /// On this computer, pass [`host_machine`]. A page passes its own clock instead, because
    /// `wasm32-unknown-unknown` has no standard-library clock and reading one there panics.
    #[must_use]
    pub fn configure_machine(
        mut self,
        configure: impl FnOnce(&Module, &mut Vm),
    ) -> InterpreterBackend {
        configure(&self.module, &mut self.vm);
        self
    }

    /// This backend, for a host that polls it between requests, as `lamella_dap::serve_polled` does:
    /// a continue leaves the program running, each [`DebugBackend::poll`] runs it a slice, and a
    /// `pause` stops it wherever it is -- a program that never stops included.
    ///
    /// A host that does not poll must not ask for this: nothing would run the program.
    #[must_use]
    pub fn polled(mut self) -> InterpreterBackend {
        self.polled = true;
        self
    }

    fn apply_breakpoints(&mut self) {
        if let Some(threads) = self.threads.as_mut() {
            threads.set_breakpoints(&self.breakpoints);
        }
    }

    /// The selected thread's session, while that thread has not finished.
    fn session(&self) -> Option<&Session> {
        self.threads.as_ref()?.session(self.selected)
    }

    /// Runs the program one slice for what is pending, and says what came of it. A stop selects the
    /// thread it happened on.
    fn run_slice(&mut self) -> Outcome {
        let InterpreterBackend {
            threads,
            module,
            vm,
            pending,
            selected,
            reported,
            exit_code,
            fault_exit_code,
            polled,
            ..
        } = self;
        let Some(threads) = threads.as_mut() else {
            return Outcome::Stopped(Stop::Done);
        };
        let step = match *pending {
            Pending::Nothing => return Outcome::Waiting,
            Pending::Resume => None,
            Pending::Step(thread) => Some(thread),
        };
        let watch = Watch {
            step,
            budget: SLICE_BUDGET,
            until: if *polled {
                vm.now_millis().map(|now| now.saturating_add(SLICE_MILLIS))
            } else {
                None
            },
        };
        let stop = match threads.debug_run(module, vm, &watch) {
            Ok(Slice::Running) => return Outcome::Running,
            Ok(Slice::Waiting) => return Outcome::Waiting,
            Ok(Slice::Breakpoint(thread)) => {
                *selected = thread;
                *reported = Some(thread);
                Stop::Breakpoint
            }
            Ok(Slice::Stepped(thread)) => {
                let at_breakpoint = !threads.is_waiting(thread)
                    && threads.session(thread).is_some_and(Session::is_at_breakpoint);
                if at_breakpoint {
                    *reported = Some(thread);
                    Stop::Breakpoint
                } else {
                    Stop::Step
                }
            }
            Ok(Slice::Finished(value)) => {
                *exit_code = exit_code_of(&value);
                Stop::Done
            }
            Err(trap) => {
                *fault_exit_code = Some(trap.exit_code());
                Stop::Fault(format!("{trap}"))
            }
        };
        *pending = Pending::Nothing;
        Outcome::Stopped(stop)
    }

    /// Runs the program for what is now pending: to its stop, slice after slice, for a host that does
    /// not poll; one slice for one that does. `Running` while it runs on.
    fn run_pending(&mut self) -> Stop {
        loop {
            match self.run_slice() {
                Outcome::Stopped(stop) => return stop,
                Outcome::Waiting => return Stop::Running,
                Outcome::Running if self.polled => return Stop::Running,
                Outcome::Running => {}
            }
        }
    }

    /// The ip of the call a frame at `ip` in `method` is inside: the instruction before `ip`.
    ///
    /// A frame that called another is past its call, and so is a thread waiting inside one; .NET
    /// shows both at the call, on its line.
    fn call_site(&self, method: MethodId, ip: u32) -> u32 {
        if !lamella_cil_runtime::interp::IP_IS_IL_OFFSET {
            return ip.saturating_sub(1);
        }
        self.offsets(method)
            .and_then(|offsets| offsets.iter().rev().find(|&&offset| offset < ip).copied())
            .unwrap_or(ip)
    }

    /// The CIL of a loaded method, or `None` for an intrinsic or unknown method.
    ///
    /// Through the module's own accessor rather than `method_body`, because which of the two body
    /// representations exists is decided by a feature `lamella-cil-runtime` declares -- so a `cfg`
    /// here could not express it: naming `method_body` outright fails to compile whenever anything
    /// in the build forwards `code-in-place`, which `lamella-wasm`'s `bake` does. Borrowed on the
    /// ordinary build; owned only
    /// where the module holds bytes rather than a decoded body.
    fn method_code(&self, method: u32) -> Option<Cow<'_, [Instruction]>> {
        self.module.method_instructions(method)
    }

    /// The CIL byte offset of each instruction in `method` (index -> offset), recomputed
    /// from the decoded body to align with the Portable PDB's sequence points.
    fn offsets(&self, method: MethodId) -> Option<Vec<u32>> {
        lamella_cil::instruction_offsets(&self.method_code(method)?)
    }

    /// The CIL byte offset of the ip value `ip` in `method`.
    ///
    ///
    /// That build used not to compile at all, on an unrelated missing accessor. **Restoring the
    /// accessor removed the only thing standing in front of this**, which is why the branch lands
    /// with it rather than after it.
    fn ip_to_il_offset(&self, method: MethodId, ip: u32) -> Option<u32> {
        if lamella_cil_runtime::interp::IP_IS_IL_OFFSET {
            return Some(ip);
        }
        self.offsets(method)?.get(ip as usize).copied()
    }

    /// The ip value naming CIL byte offset `il_offset` in `method`: under `code-in-place` the
    /// offset itself, otherwise the instruction index at that boundary -- else the last
    /// instruction at or before it.
    fn il_offset_to_ip(&self, method: MethodId, il_offset: u32) -> Option<u32> {
        if lamella_cil_runtime::interp::IP_IS_IL_OFFSET {
            return Some(il_offset);
        }
        let offsets = self.offsets(method)?;
        let slice = offsets.get(..offsets.len().saturating_sub(1))?;
        let index = slice
            .iter()
            .position(|&offset| offset == il_offset)
            .or_else(|| slice.iter().rposition(|&offset| offset <= il_offset))?;
        u32::try_from(index).ok()
    }

    /// Source names for `method`'s locals by slot, from the PDB, when available.
    fn local_names(&self, method: MethodId) -> Option<BTreeMap<u16, String>> {
        let pdb = PortablePdb::read(self.pdb.as_ref()?).ok()?;
        let method_rid = *self.method_rid.get(&method)?;
        let mut names = BTreeMap::new();
        for variable in pdb.local_variables(method_rid) {
            names.insert(variable.index, String::from(variable.name));
        }
        Some(names)
    }

    /// The zero `method`'s local signature gives local `slot`, where its type decides one.
    fn local_zero(&self, method: MethodId, slot: usize) -> Option<LocalZero> {
        self.local_zeros.get(&method)?.get(slot).copied().flatten()
    }

    /// The struct of type `type_id` with every field at its zero, as a stored struct of that type is
    /// held.
    fn zero_struct(&self, type_id: TypeId) -> Value {
        Value::Struct(self.module.type_field_defaults(type_id).unwrap_or_default().into_boxed_slice())
    }

    /// Local `slot` of `method`, holding `value`, as a variables view shows it.
    ///
    /// A local the method has not stored yet can hold a 32-bit 0 whatever its type: a frame starts a
    /// struct's slot there, and every slot the loader recorded no kind for. Under any class but `int`
    /// such a 0 is only that, since a stored `long` is 64 bits and a stored reference a reference or
    /// null, so it is shown as its class's zero, the value the local holds; and a struct's, since a
    /// stored struct is held as one, as that struct with its fields at zero. A local whose zero
    /// nothing decides is shown as held: an enum's 0 is that same 32-bit 0, stored or not.
    fn shown_local(&self, method: MethodId, slot: usize, value: &Value) -> Value {
        match self.local_zero(method, slot) {
            Some(LocalZero::Class(class)) if class != StackClass::Int32 && *value == Value::Int32(0) => zero(class),
            Some(LocalZero::Struct(type_id)) if *value == Value::Int32(0) => self.zero_struct(type_id),
            _ => value.clone(),
        }
    }

    /// A row for each local `names` names in `method` at or past slot `stored`, the count of slots
    /// the frame holds, with that slot: locals the method has not stored yet, at their type's zero,
    /// or as not assigned yet where nothing decides it.
    fn unstored_locals(&self, method: MethodId, stored: usize, names: &BTreeMap<u16, String>) -> Vec<(usize, Variable)> {
        names
            .iter()
            .filter(|&(&slot, name)| usize::from(slot) >= stored && !name.is_empty())
            .map(|(&slot, name)| {
                let (value, kind) = match self.local_zero(method, usize::from(slot)) {
                    Some(LocalZero::Class(class)) => format_value(&self.vm, zero(class)),
                    Some(LocalZero::Struct(type_id)) => format_value(&self.vm, self.zero_struct(type_id)),
                    None => (NOT_ASSIGNED_YET.to_owned(), String::new()),
                };
                (usize::from(slot), Variable { name: name.clone(), value, kind })
            })
            .collect()
    }

    /// The rows of one scope of frame `frame_index`, counted as [`DebugBackend::stack`] lists the
    /// frames, each with the slot it shows. Locals take their names from the PDB, arguments from
    /// the method's parameter metadata (`this` first for an instance method), and anything unnamed
    /// `localN` / `argN` / `stackN`. Where the PDB names a method's locals, a local it does not
    /// name, a compiler's temporary, is not listed, as .NET's debugger lists only the locals the
    /// PDB's scopes name.
    fn rows(&self, frame_index: usize, scope: Scope) -> Vec<(usize, Variable)> {
        let Some(session) = self.session() else {
            return Vec::new();
        };
        let Some(frame) = self
            .session_frame(frame_index)
            .and_then(|at| session.frame(at))
        else {
            return Vec::new();
        };
        let method = frame.method;
        let (prefix, values) = match scope {
            Scope::Arguments => ("arg", frame.args),
            Scope::Locals => ("local", frame.locals),
            Scope::Stack => ("stack", frame.stack),
        };
        let names = matches!(scope, Scope::Locals)
            .then(|| self.local_names(method))
            .flatten();
        let arguments = matches!(scope, Scope::Arguments);
        let locals = matches!(scope, Scope::Locals);
        let mut rows: Vec<(usize, Variable)> = values
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                let name = match names.as_ref() {
                    Some(names) => names.get(&(index as u16)).filter(|name| !name.is_empty())?.clone(),
                    None => arguments
                        .then(|| self.module.arg_name(method, index).map(String::from))
                        .flatten()
                        .unwrap_or_else(|| format!("{prefix}{index}")),
                };
                let shown = if locals { self.shown_local(method, index, value) } else { value.clone() };
                let (text, kind) = format_value(&self.vm, shown);
                Some((index, Variable { name, value: text, kind }))
            })
            .collect();
        if let Some(names) = names.as_ref().filter(|_| locals) {
            rows.extend(self.unstored_locals(method, values.len(), names));
        }
        rows
    }

    /// The session's index for frame `frame`, counted as [`DebugBackend::stack`] lists the frames:
    /// `frame` 0 is the innermost, while the session counts from the entry frame.
    fn session_frame(&self, frame: usize) -> Option<usize> {
        self.session()?
            .depth()
            .checked_sub(frame.checked_add(1)?)
    }
}

/// This computer's clock, for a program debugged on it: the one `lamella run` gives a program, so
/// `DateTime.Now` reads the date, `Environment.TickCount` the milliseconds since the computer
/// started, and `Thread.Sleep` waits. Pass it to [`InterpreterBackend::configure_machine`].
///
/// Not built for `wasm32`, which has no standard-library clock.
#[cfg(not(target_arch = "wasm32"))]
pub fn host_machine(module: &Module, vm: &mut Vm) {
    lamella_wire_host::engine::install_host_clock(vm);
    let ticks = vm.now_ticks();
    lamella_cil_runtime::set_wall_clock(module, vm, ticks);
}

impl DebugBackend for InterpreterBackend {
    fn launch(&mut self) -> Result<(), String> {
        let args = lamella_cil_runtime::entry_args(&self.module, &mut self.vm, self.entry);
        match Threads::new(&self.module, self.entry, args) {
            Ok(threads) => {
                self.threads = Some(threads);
                self.reported = None;
                self.selected = 0;
                self.pending = Pending::Nothing;
                self.fault_exit_code = None;
                self.apply_breakpoints();
                Ok(())
            }
            Err(trap) => Err(format!("the program could not be started: {trap}")),
        }
    }

    /// The program runs on the interpreter in this process, so the adapter's own evaluation session
    /// runs where the program does.
    fn runs_in_this_process(&self) -> bool {
        true
    }

    /// Every thread runs on until one reaches a breakpoint or the program ends. Run inside the call for
    /// a host that does not poll; left to [`DebugBackend::poll`] for one that does.
    fn resume(&mut self) -> Stop {
        let Some(threads) = self.threads.as_mut() else {
            return Stop::Done;
        };
        if let Some(thread) = self.reported.take() {
            threads.step_off(thread);
        }
        self.pending = Pending::Resume;
        if self.polled {
            return Stop::Running;
        }
        self.run_pending()
    }

    /// The selected thread runs one instruction, and every other thread runs meanwhile, as the
    /// scheduler interleaves them: a thread that waits -- on a sleep, another thread, a lock -- takes
    /// its one instruction once the wait is over. The adapter composes every source-level step from
    /// this one. A step of a thread that has finished goes on as a continue, as the program does on
    /// .NET once the thread it was stepping ends.
    fn step(&mut self) -> Stop {
        let Some(threads) = self.threads.as_mut() else {
            return Stop::Done;
        };
        if let Some(thread) = self.reported.take() {
            threads.step_off(thread);
        }
        self.pending = if threads.session(self.selected).is_some() {
            Pending::Step(self.selected)
        } else {
            Pending::Resume
        };
        self.run_pending()
    }

    /// The program runs on for one more slice, after a call answered [`Stop::Running`].
    fn poll(&mut self) -> Stop {
        match self.run_slice() {
            Outcome::Stopped(stop) => stop,
            Outcome::Running | Outcome::Waiting => Stop::Running,
        }
    }

    /// Stops the program where it is, which needs nothing more than not running it: it runs only
    /// inside this backend's calls. The thread the scheduler ran last is the one the stop names.
    fn pause(&mut self) -> bool {
        self.pending = Pending::Nothing;
        if let Some(thread) = self.threads.as_ref().and_then(Threads::current) {
            self.selected = thread;
        }
        true
    }

    fn exit_code(&self) -> i32 {
        self.exit_code
    }

    fn fault_exit_code(&self) -> Option<i32> {
        self.fault_exit_code
    }

    fn depth(&self) -> usize {
        self.session().map_or(0, Session::depth)
    }

    /// The threads that have not finished, by the scheduler's id plus one, so the main thread is 1,
    /// each named as .NET's debugger names it: the main thread, a thread by the `Thread.Name` its
    /// program gave it, and an unnamed one.
    fn threads(&self) -> Vec<ThreadInfo> {
        let Some(threads) = self.threads.as_ref() else {
            return vec![ThreadInfo { id: 1, name: String::from(MAIN_THREAD_NAME) }];
        };
        threads
            .ids()
            .into_iter()
            .map(|thread| ThreadInfo {
                id: thread + 1,
                name: if thread == 0 {
                    String::from(MAIN_THREAD_NAME)
                } else {
                    threads
                        .thread_name(&self.module, &self.vm, thread)
                        .unwrap_or_else(|| String::from(UNNAMED_THREAD_NAME))
                },
            })
            .collect()
    }

    fn selected_thread(&self) -> u32 {
        self.selected + 1
    }

    fn select_thread(&mut self, id: u32) -> bool {
        let Some(thread) = id.checked_sub(1) else {
            return false;
        };
        let known = match self.threads.as_ref() {
            Some(threads) => threads.session(thread).is_some(),
            None => thread == 0,
        };
        if known {
            self.selected = thread;
        }
        known
    }

    /// Always `Ok`: the breakpoint set is a list the session owns, there is no unit with a
    /// capacity and no wire to lose, so there is no way for the programming to fail. An
    /// unbounded backend is the case the seam's promise is trivially true for.
    fn set_breakpoints(&mut self, addresses: &[u64]) -> Result<(), String> {
        self.breakpoints = addresses
            .iter()
            .map(|&address| decode_address(address))
            .collect();
        self.apply_breakpoints();
        Ok(())
    }

    fn stack(&self) -> Vec<Frame> {
        let Some(session) = self.session() else {
            return Vec::new();
        };
        (0..session.depth())
            .rev()
            .filter_map(|index| {
                session.frame(index).map(|frame| Frame {
                    address: encode_address(frame.method, frame.ip),
                    name: self
                        .module
                        .method_name(frame.method)
                        .map(String::from)
                        .unwrap_or_else(|| alloc_method_name(frame.method, frame.ip)),
                    line: frame.ip + 1,
                })
            })
            .collect()
    }

    /// Frame `index`'s source location, AT THE CALL for a frame inside one: every frame but the
    /// innermost, and the innermost of a thread waiting inside the call that blocks it. Such a
    /// frame's address is the instruction after its call, which can be the next statement's first --
    /// and a frame shown there reads as having finished the line it is in.
    fn frame_source_location(&self, index: usize, address: u64) -> Option<SourceLocation> {
        let waiting = self
            .threads
            .as_ref()
            .is_some_and(|threads| threads.is_waiting(self.selected));
        if index == 0 && !waiting {
            return self.source_location(address);
        }
        let (method, ip) = decode_address(address);
        self.source_location(encode_address(method, self.call_site(method, ip)))
    }

    fn resolve_source_breakpoint(&self, document: &str, line: u32) -> Option<u64> {
        let pdb = PortablePdb::read(self.pdb.as_ref()?).ok()?;
        let basename: fn(&str) -> &str = |path| path.rsplit(['/', '\\']).next().unwrap_or(path);
        let (method_rid, il_offset) = pdb.resolve_breakpoint(document, line).or_else(|| {
            let target = basename(document);
            let document = (1..=pdb.method_count())
                .filter_map(|rid| pdb.method_document(rid))
                .find(|candidate| basename(candidate) == target)?;
            pdb.resolve_breakpoint(&document, line)
        })?;
        let method = self
            .module
            .resolve(self.module.method_asm(self.entry), Token::new(METHOD_DEF, method_rid))?;
        let ip = self.il_offset_to_ip(method, il_offset)?;
        Some(encode_address(method, ip))
    }

    fn source_location(&self, address: u64) -> Option<SourceLocation> {
        let (method, index) = decode_address(address);
        let method_rid = *self.method_rid.get(&method)?;
        let il_offset = self.ip_to_il_offset(method, index)?;
        let pdb = PortablePdb::read(self.pdb.as_ref()?).ok()?;
        let point = pdb.source_location(method_rid, il_offset)?;
        Some(SourceLocation {
            file: pdb.method_document(method_rid)?,
            line: point.start_line,
            column: point.start_column,
            end_line: point.end_line,
            end_column: point.end_column,
        })
    }

    fn step_budget(&self) -> usize {
        1_000_000
    }

    fn has_source(&self) -> bool {
        self.pdb.is_some()
    }

    fn at_source_boundary(&self) -> bool {
        let Some(session) = self.session() else {
            return false;
        };
        let Some(frame) = session
            .depth()
            .checked_sub(1)
            .and_then(|innermost| session.frame(innermost))
        else {
            return false;
        };
        let Some(boundaries) = self.seq_boundaries.get(&frame.method) else {
            return false;
        };
        self.ip_to_il_offset(frame.method, frame.ip)
            .is_some_and(|il_offset| boundaries.contains(&il_offset))
    }

    fn variables(&self, frame_index: usize, scope: Scope) -> Vec<Variable> {
        self.rows(frame_index, scope).into_iter().map(|(_, variable)| variable).collect()
    }

    fn can_set_variables(&self) -> bool {
        true
    }

    fn set_variable(
        &mut self,
        frame_index: usize,
        scope: Scope,
        name: &str,
        value: &str,
    ) -> Option<String> {
        if matches!(scope, Scope::Stack) {
            return None;
        }
        let slot = self
            .rows(frame_index, scope)
            .into_iter()
            .find(|(_, variable)| variable.name == name)?
            .0;
        let at = self.session_frame(frame_index)?;
        let frame = self.session()?.frame(at)?;
        let current = match scope {
            Scope::Arguments => frame.args.get(slot).cloned(),
            Scope::Locals => frame
                .locals
                .get(slot)
                .map(|value| self.shown_local(frame.method, slot, value)),
            Scope::Stack => None,
        }?;
        let new_value = match &current {
            Value::Object(reference) if self.vm.heap().as_string(*reference).is_some() => {
                let text = strip_one_quote_pair(value);
                Value::Object(self.vm.heap_mut().alloc_text(text))
            }
            _ => parse_value(&current, value)?,
        };
        let session = self.threads.as_mut()?.session_mut(self.selected)?;
        let written = match scope {
            Scope::Arguments => session.set_arg(at, slot, new_value.clone()),
            Scope::Locals => session.set_local(at, slot, new_value.clone()),
            Scope::Stack => false,
        };
        if !written {
            return None;
        }
        Some(format_value(&self.vm, new_value).0)
    }

    fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
        Vec::new()
    }

    fn read_registers(&self) -> Vec<Register> {
        Vec::new()
    }

    fn disassemble(&self, address: u64, offset: i64, count: usize) -> Vec<Disassembled> {
        let (method, base_ip) = decode_address(address);
        let code = self.method_code(method);
        let code = code.as_deref();
        (0..count)
            .map(|step| {
                let ip = i64::from(base_ip) + offset + step as i64;
                let address = if ip >= 0 {
                    encode_address(method, ip as u32)
                } else {
                    0
                };
                let text = match (code, usize::try_from(ip)) {
                    (Some(code), Ok(index)) if index < code.len() => {
                        format_instruction(&code[index])
                    }
                    _ => "(out of range)".to_owned(),
                };
                Disassembled { address, text }
            })
            .collect()
    }

    fn take_output(&mut self) -> Option<String> {
        let output = self.vm.output();
        if output.len() > self.output_sent {
            let text = String::from_utf16_lossy(&output[self.output_sent..]);
            self.output_sent = output.len();
            Some(text)
        } else {
            None
        }
    }

    fn take_debug_output(&mut self) -> Option<String> {
        let output = self.vm.debug_output();
        if output.len() > self.debug_sent {
            let text = String::from_utf16_lossy(&output[self.debug_sent..]);
            self.debug_sent = output.len();
            Some(text)
        } else {
            None
        }
    }
}

/// Encodes a CIL location `(method, instruction)` as one opaque address: the method id
/// in the high 32 bits, the instruction index in the low 32.
#[must_use]
pub fn encode_address(method: MethodId, instruction: u32) -> u64 {
    (u64::from(method) << 32) | u64::from(instruction)
}

/// The inverse of [`encode_address`].
#[must_use]
pub fn decode_address(address: u64) -> (u32, u32) {
    ((address >> 32) as u32, (address & 0xFFFF_FFFF) as u32)
}

fn alloc_method_name(method: MethodId, ip: u32) -> String {
    format!("method#{method} @{ip}")
}

/// What a local of type `local` holds before its method first stores it, where its signature or the
/// module decides it: its stack class's zero ([`SigType::stack_class`]), or, for a value type named
/// by token that the module loaded as a struct and not an enum, that struct with its fields at zero.
/// `None` for an enum, whose 0 is the integer its slot holds, and for a type whose zero this does
/// not reach: a generic parameter, or a generic struct's instantiation. `asm` is the program's
/// assembly, whose tokens the signature uses.
fn local_type_zero(module: &Module, asm: u8, local: &SigType) -> Option<LocalZero> {
    if let Some(class) = local.stack_class() {
        return Some(LocalZero::Class(class));
    }
    let SigType::ValueType(token) = local else {
        return None;
    };
    let type_id = module.type_id_of(asm, *token)?;
    let is_enum = module.type_handle_of(type_id).is_some_and(|handle| module.is_enum_by_handle(handle));
    (module.type_is_value_type(type_id) && !is_enum).then_some(LocalZero::Struct(type_id))
}

/// The value a local of `class` holds before its method first stores it: every local starts zeroed.
fn zero(class: StackClass) -> Value {
    match class {
        StackClass::Int32 => Value::Int32(0),
        StackClass::Int64 => Value::Int64(0),
        StackClass::NativeInt => Value::NativeInt(0),
        StackClass::Float32 => Value::Single(0.0),
        StackClass::Float64 => Value::Float(0.0),
        StackClass::ObjectRef => Value::Null,
    }
}

/// What a variables view says of a local the method has not stored yet whose class its type does
/// not decide.
const NOT_ASSIGNED_YET: &str = "<not assigned yet>";

/// Renders a value as `(display text, type name)` for the variables view.
fn format_value(vm: &Vm, value: Value) -> (String, String) {
    match value {
        Value::Int32(n) => (n.to_string(), "int".to_owned()),
        Value::Int64(n) => (n.to_string(), "long".to_owned()),
        Value::NativeInt(n) => (n.to_string(), "native int".to_owned()),
        Value::Float(f) => (f.to_string(), "double".to_owned()),
        Value::Single(f) => (f.to_string(), "float".to_owned()),
        Value::Object(reference) => match vm.heap().as_string(reference) {
            Some(chars) => (
                format!("\"{}\"", String::from_utf16_lossy(&chars)),
                "string".to_owned(),
            ),
            None => ("object".to_owned(), "object".to_owned()),
        },
        Value::Null => ("null".to_owned(), "object".to_owned()),
        Value::Struct(fields) => (format!("struct[{}]", fields.len()), "struct".to_owned()),
        Value::ByRef(_) => ("&".to_owned(), "byref".to_owned()),
        Value::TypedRef { .. } => ("typedref".to_owned(), "typedref".to_owned()),
    }
}

/// Parses `text` into a [`Value`] of the SAME kind as `current` -- the inverse of
/// [`format_value`]'s rendering, so a `setVariable` round-trips. Returns `None` when the
/// text does not parse as that kind, or when the kind is one this editor does not support
/// (null / struct / byref / typedref, plus a NON-string object -- see the trailing arms):
/// those render as a description, not an editable literal, so there is nothing to parse back
/// into. A managed-`String` object IS editable, but `set_variable` handles it before reaching
/// here (it mints a new String rather than parsing), so the `Value::Object` arm here only ever
/// covers non-string objects.
///
/// `bool` and `char` are not distinguished here: the runtime widens both to [`Value::Int32`]
/// (see `Value`), and `format_value` renders that slot as a plain decimal, so the inverse is
/// an integer parse -- typing `1` into a `bool` local sets it to 1, matching how it reads back.
fn parse_value(current: &Value, text: &str) -> Option<Value> {
    let text = text.trim();
    match current {
        Value::Int32(_) => text.parse::<i32>().ok().map(Value::Int32),
        Value::Int64(_) => text.parse::<i64>().ok().map(Value::Int64),
        Value::NativeInt(_) => text.parse::<i64>().ok().map(Value::NativeInt),
        Value::Float(_) => text.parse::<f64>().ok().map(Value::Float),
        Value::Single(_) => text.parse::<f32>().ok().map(Value::Single),
        Value::Object(_)
        | Value::Null
        | Value::Struct(_)
        | Value::ByRef(_)
        | Value::TypedRef { .. } => None,
    }
}

/// Strips one matched pair of surrounding double-quotes from `text`, if present, returning
/// the inner content; otherwise returns `text` unchanged. This is the inverse of
/// [`format_value`]'s `"hi"` rendering of a string, so a `setVariable` round-trips whether the
/// client sends the rendered `"hi"` or the bare `hi`. Only the outermost pair is removed and
/// the content is taken verbatim (no trimming or unescaping): `"a b"` -> `a b`, `"` -> `"`
/// (a lone quote has no pair), `""` -> `` (empty string).
fn strip_one_quote_pair(text: &str) -> &str {
    text.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(text)
}

/// Renders one CIL instruction as a mnemonic and its operand, for disassembly.
fn format_instruction(instruction: &Instruction) -> String {
    let mnemonic = instruction.opcode.mnemonic();
    match &instruction.operand {
        Operand::None => mnemonic.to_owned(),
        Operand::Int8(value) => format!("{mnemonic} {value}"),
        Operand::Int32(value) => format!("{mnemonic} {value}"),
        Operand::Int64(value) => format!("{mnemonic} {value}"),
        Operand::Float32(value) => format!("{mnemonic} {value}"),
        Operand::Float64(value) => format!("{mnemonic} {value}"),
        Operand::Variable(slot) => format!("{mnemonic} {slot}"),
        Operand::Target(target) => format!("{mnemonic} -> {target}"),
        Operand::Switch(targets) => format!("{mnemonic} ({} targets)", targets.len()),
        Operand::Token(token) => format!("{mnemonic} 0x{:08X}", token.0),
        Operand::Alignment(align) => format!("{mnemonic} {align}"),
    }
}
