//! A tree-of-frames CIL interpreter over a hand-built method body.

#[cfg(feature = "bcl")]
use crate::intrinsic_registry::intrinsic_id;
use crate::module::{
    CastElem, CastPrim, FieldOffsets, IntrinsicType, MethodId, MethodKind, Module, SlotKind, TypeId,
    asm_key,
};
use crate::object::{Heap, ObjectRef, PrimKind};
use crate::trap::{Trap, UnhandledException};
use crate::value::{Location, Value};
use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_cil::{
    EhClause, EhKind, Instruction, InstructionRange, MethodBodyImage, Opcode, Operand,
};
use lamella_pin_events::PinEvent;
use lamella_token::Token;

/// The maximum depth of the call stack before [`Trap::CallStackOverflow`]. Bounds
/// runaway recursion in the absence of a configured stack size.
const MAX_CALL_DEPTH: usize = 4096;

/// One marshaled P/Invoke argument crossing the host seam: a 64-bit scalar slot (every
/// integer/pointer parameter rides one), or a byte buffer the callee reads through a
/// pointer (an ANSI string, NUL-terminated by the marshaler; the host passes its address).
pub enum PInvokeArg {
    /// An integer value, sign-extended into the call ABI's 64-bit argument slot.
    Scalar(i64),
    /// A byte buffer passed by address (the buffer outlives the call).
    Bytes(Vec<u8>),
}

/// The host's native-call hook: resolve `target`'s library + entry point and invoke it
/// with `args`, returning the raw 64-bit result slot. `Err(())` reports a resolution or
/// call failure (missing library/symbol, unsupported arity).
pub type PInvokeHostFn = fn(&crate::module::PInvokeTarget, &mut [PInvokeArg]) -> Result<i64, ()>;

/// The embedder's pin-change event queue, as the two operations the interpreter performs on it.
///
/// Plain fn pointers like the wall-clock sink, so a bare-metal boot path can register one without
/// an allocation or a trait object. The queue ITSELF is the embedder's: a serve firmware keeps it
/// in `.bss`, and an ahead-of-time compiled image keeps it in its caller-provided RAM region,
/// because the archive such an image links has no writable-data segment and a `static` there
/// resolves into `.text`. Both drive the same `lamella_pin_events::PinEventQueue`, and this is
/// where its two ends meet.
#[derive(Debug, Clone, Copy)]
pub struct PinEventSource {
    /// Takes the oldest queued event, or `None` when nothing is waiting.
    pub next: fn() -> Option<PinEvent>,
    /// Whether the queue has DROPPED anything since the last call, clearing the flag.
    ///
    /// Separate from [`PinEventSource::next`] because loss is a property of the DRAIN and not of
    /// any event that survived it: a burst that overflowed is one gap in the sequence, not one gap
    /// per survivor.
    pub drain_overflowed: fn() -> bool,
}

/// The runtime context an execution shares across frames and exposes to
/// intrinsics: the managed heap and the console output.
///
/// This is the `Vm` an [`crate::module::IntrinsicFn`] receives. It deliberately
/// does *not* hold the RUNNING call frames or the program, so an intrinsic can borrow it
/// mutably while the interpreter holds the frame stack. The console is an
/// in-memory buffer for now; a device console transport replaces it later.
///
/// The one exception proves the rule rather than bending it: an activation SUSPENDED beneath a
/// nested run parks its frames here ([`ParkedFrames`]) because nothing else can reach them to mark
/// them. They are moved in and back out, so no borrow is held and the property above survives --
/// the frames the interpreter is currently executing are still not here.
#[derive(Debug, Default)]
pub struct Vm {
    heap: Heap,
    /// Recycled activation-frame shells and argument buffers ([`FramePool`]): on a
    /// bump-allocator device tier a dropped `Vec`'s storage never comes back, so without
    /// recycling EVERY managed call costs permanent arena (~200-300 B measured). Owned by
    /// the `Vm` so every session and green thread shares one pool.
    frame_pool: FramePool,
    /// The console-output bound ([`Vm::set_output_cap`]): `None` = unbounded (host).
    output_cap: Option<usize>,
    /// Whether the cap clipped a write since last taken ([`Vm::take_output_clipped`]).
    output_clipped: bool,
    output: Vec<u16>,
    /// The DEBUG channel -- the bytes `System.Diagnostics.Debug`'s `DefaultTraceListener`
    /// emits, kept separate from `output` (the Console). Conceptually the developer's debug
    /// sink: a host renders it to STDERR, distinct from Console.Out's STDOUT, so a bare
    /// `Debug.WriteLine` (no listener config) is seen without polluting program output. Empty
    /// unless `debug_write` is called.
    debug_output: Vec<u16>,
    statics: Vec<Value>,
    /// The types whose `.cctor` has run (or is running) in this VM -- the lazy-initialization
    /// state of II.10.5.3. Marked BEFORE the cctor's frame is pushed, so a recursive trigger
    /// inside the cctor sees the type as initialized (partial state, as the spec allows)
    /// instead of re-running it.
    cctors_run: BTreeSet<TypeId>,
    /// The types whose `.cctor` THREW, and the `TypeInitializationException` raised for each.
    ///
    /// [`cctors_run`](Self::cctors_run) alone cannot answer II.10.5.3.1 guarantee 3: it records
    /// that an initializer RAN and not how it ENDED, so a failed type would read as initialized
    /// and the next access would reach statics nobody assigned. This map is what closes that.
    ///
    /// The exception object is stored, not rebuilt: .NET caches the wrapper it raised and rethrows
    /// that same instance at every later access, so a program comparing two catches by reference
    /// sees one object.
    #[cfg(feature = "exceptions")]
    cctors_failed: Vec<(TypeId, ObjectRef)>,
    /// The message string of each exception object, kept as runtime side-state so
    /// `Exception.Message` works without modeling mscorlib's field layout (and so the
    /// message-stripping knob has one place to act). Keyed by the exception object.
    exception_messages: BTreeMap<ObjectRef, ObjectRef>,
    /// The exception each exception WRAPS, for `Exception.InnerException` -- the same side-state
    /// as `exception_messages` and for the same reason: a runtime-raised exception has no field
    /// storage, so the VES cannot assign the field a managed constructor would have. Populated
    /// today only by a failed type initializer, which is the one wrapper the VES mints itself.
    exception_inners: BTreeMap<ObjectRef, ObjectRef>,
    /// The base-chain TAG vector of a runtime-fault exception object -- one whose managed
    /// type is external to the loaded module (`EXTERNAL_TYPE_ID`), so it has no live base
    /// chain to walk. A catchable fault (`catchable_fault`) records the vector of its .NET
    /// exception type here (leaf-first up to `System.Object`), so the handler search tests a
    /// `catch`'s tag against it exactly as it does a managed exception's
    /// [`Module::exception_base_chain`]. Keyed by the fault object; empty/absent without the
    /// `exceptions` feature.
    ///
    #[cfg(feature = "exceptions")]
    exception_faults: Vec<(ObjectRef, FaultIdentity)>,
    /// Whether a finalizer is currently running: the collector is paused (a GC triggered
    /// from within a finalizer is a no-op) so the in-flight f-reachable list stays valid.
    #[cfg(feature = "finalizers")]
    finalizing: bool,
    /// Set by `GC.Collect`: the next safepoint collects regardless of the heap threshold.
    #[cfg(feature = "gc")]
    force_collect: bool,
    /// A nesting depth that suspends collection while non-zero. A reflective intrinsic that
    /// builds objects across a nested interpreter run (`GetCustomAttributes` instantiates each
    /// attribute by running its constructor) raises this for the duration: the moving collector
    /// remaps only roots it can reach through the running session's frames, so an instance the
    /// intrinsic holds in a Rust local -- or the result array it accumulates into -- would
    /// otherwise dangle if a nested constructor crossed a GC safepoint. Balanced by
    /// [`Vm::suspend_collection`] / [`Vm::resume_collection`].
    #[cfg(feature = "gc")]
    collect_suspend: u32,
    /// Frames of activations suspended on the Rust stack across a nested run ([`ParkedFrames`]).
    /// Suspension is the wrong instrument for those: it stops the collection rather than teaching
    /// it what is live, so a nested run that allocates heavily has nowhere to give memory back.
    #[cfg(feature = "gc")]
    parked: ParkedFrames,
    /// The exception currently propagating without (yet) a handler: set the moment a
    /// `throw`/`rethrow` begins its search and cleared the instant a `catch`/`filter`
    /// accepts it, so if the search exhausts the call stack this still holds the culprit.
    /// A [`Session`] armed with [`Session::set_pause_on_unhandled_exception`] reads it to
    /// report the object on a [`StopReason::Exception`] pause. Always `None` (and never set)
    /// without the `exceptions` feature.
    #[cfg(feature = "exceptions")]
    unhandled: Option<ObjectRef>,
    /// The wall clock, ANCHORED to the monotonic clock: `(monotonic_ms_at_set, ticks_at_set)`,
    /// where ticks are 100-nanosecond units since the .NET epoch (0001-01-01 00:00:00). The
    /// interpreter core is no_std and has no clock of its own -- the embedder sets the wall time
    /// (the host from `std::time`, a device from its RTC, or a synced `SystemClock`) via
    /// [`Vm::set_now_ticks`], which records it against the current monotonic reading so
    /// `DateTime.Now`/`UtcNow` (through `datetime_now_ticks`) ADVANCE with elapsed monotonic time
    /// rather than freezing at the set instant. `None` until set -> the epoch (a defined value,
    /// not garbage), so an unconfigured build is unchanged.
    wall_anchor: Option<(u64, i64)>,
    /// The embedder's observer for wall-clock SETS ([`Vm::set_wall_clock_sink`]): called on every
    /// set of the managed wall clock -- through [`set_wall_clock`], an RTC load, a managed `Seed`,
    /// a completed SNTP or NTS sync -- with `Some(ticks)`, or with `None` when the clock returns to
    /// never-set. So a device embedder can MIRROR the one managed wall clock into an engine that
    /// reads time through its own registered source (the device TLS engine's `set_time_source`). A
    /// plain fn pointer, like the clock seam, so a bare-metal boot path can register it. `None` = no
    /// observer (the default; nothing extra happens on a set).
    wall_sink: Option<fn(Option<i64>)>,
    /// The last state the sink was told, anchored like `wall_anchor`:
    /// `(monotonic_ms_when_told, ticks)`, or `None` for never-set. It is what a sink registered
    /// AFTER a set is handed, advanced by the time since, so wiring order never loses a set.
    wall_told: Option<(u64, i64)>,
    /// How many times the sink has been told anything. [`set_wall_clock`] compares it across the
    /// managed setter to tell a corlib that notifies from one that predates the notification.
    wall_tellings: u32,
    /// The embedder's pin-change event queue ([`Vm::set_pin_event_source`]), drained between
    /// scheduler quanta. `None` = no queue, and the drain is then not attempted at all -- which is
    /// every host build and every board that arms no interrupt.
    pin_events: Option<PinEventSource>,
    /// Whether a pin-event drain is already in progress on this `Vm`.
    ///
    /// The drain invokes managed code with [`run`], which re-enters the scheduler loop the drain
    /// itself lives in -- so without this the dispatch of event 1 drains events 2 and 3 BEFORE
    /// event 1's body has run. That is not merely wasteful: it delivers a queue in REVERSE, because
    /// each event nests inside the one before it and the innermost finishes first. It also costs a
    /// call frame per event, so a full queue arrives as a stack as deep as its capacity.
    pin_events_draining: bool,
    /// Native KEY material the managed tier references only by HANDLE (a TLS exporter output,
    /// an AEAD key): the bytes live here, runtime-side, and never appear in managed values.
    /// Append-only like the TLS handle tables; a released slot is ZEROIZED before it is
    /// dropped so key bytes do not linger on the heap.
    native_keys: Vec<Option<Vec<u8>>>,
    /// Per-socket receive timeouts in milliseconds (`Socket.ReceiveTimeout`, the SO_RCVTIMEO
    /// shape), keyed by handle; absent = infinite (the .NET default). A `Read`-interest park for
    /// a mapped handle is DEADLINE-BOUNDED: the reactor wakes the thread when the deadline passes
    /// even if the socket never readies, the re-polled op returns WouldBlock again, and the
    /// MANAGED receive loop decides expiry (throws its `SocketException`) -- the runtime only
    /// bounds the park. Entries are dropped on close so a recycled handle cannot inherit one.
    /// Per-socket timeouts, from the SHARED seam rather than a table of this crate's own:
    /// a second language binding that seam gets the same behavior instead of inventing it.
    socket_timeouts: crate::net::timeout::Timeouts,
    /// The cooperative green-thread scheduler's signal channel: a threading intrinsic
    /// (`Thread.Start` / `Join` / `Yield`) sets this to request a scheduler action; the top-level
    /// scheduler loop takes it the moment the running thread next pauses. `None` between requests.
    thread_op: Option<ThreadOp>,
    /// The next green-thread id to hand out (`Thread.Start` reserves one). Thread 0 is the main
    /// thread; spawned threads get 1, 2, ...
    next_thread_id: u32,
    /// The id of the green thread currently running (for `Thread.CurrentThread`).
    current_thread_id: u32,
    /// The number of LIVE (not-yet-finished) green threads, maintained by the scheduler. While this
    /// is > 1 a GC safepoint in `advance` DEFERS collection (a collection must root EVERY thread's
    /// frames, which only the scheduler can reach) and the scheduler collects all threads at the
    /// next switch; <= 1 means the running session's own roots suffice, so collection runs inline.
    live_thread_count: u32,
    /// The per-object `Monitor` lock table: each entered lock object, keyed by its heap index, maps
    /// to its [`LockState`] -- the owning thread, the recursion depth, and the FIFO queue of threads
    /// blocked waiting to acquire it. A held lock now spans scheduler switches (the holder can be
    /// preempted inside its critical section, with only CONTENDERS blocking) and therefore GCs, so
    /// the compactor remaps these keys at every collection ([`run_collection`]) just as it does the
    /// exception-message table.
    locks: BTreeMap<u32, LockState>,
    /// Reflection references live as metadata handles rather than heap objects. Each handle
    /// used as a monitor gets one private heap object, shared by all monitor operations.
    /// These objects remain rooted for the VM's lifetime, including while their locks are idle.
    reflection_monitors: BTreeMap<CastElem, ObjectRef>,
    /// One representative metadata handle per resolved Type identity. Returning the same
    /// handle makes raw object-reference comparisons agree with reflection equality.
    reflection_type_handles: BTreeMap<CastElem, i64>,
    /// Threads whose most recent timed `Monitor.Wait` ended by its DEADLINE rather than by a pulse.
    ///
    /// The outcome cannot be the parking intrinsic's return value: that call returns BEFORE the
    /// thread parks, and which way the wait ends is not known until it is woken. So the wait is two
    /// calls -- park, then read this -- the same shape a socket op uses when it re-polls after its
    /// park. An id is inserted only on a timeout and REMOVED when read, so a later untimed wait
    /// cannot inherit an older wait's verdict.
    wait_timeouts: BTreeSet<u32>,
    /// Threads the scheduler has run to completion, so a managed `Thread.IsAlive` can answer.
    ///
    /// The SAME shape as `wait_timeouts` above and for the same reason: the scheduler's per-thread
    /// state lives in the run loop's slots, which a seam cannot reach, so the run loop publishes the
    /// one fact a seam needs. Unlike a verdict, this is not consumed on read -- a thread stays
    /// finished, and `IsAlive` may be asked any number of times.
    ///
    finished_threads: BTreeSet<u32>,
    /// The host clock seam for timed waits (`Thread.Sleep`, the scheduler's idle-block). `now_millis`
    /// reads a monotonic millisecond count; `sleep_millis` blocks the OS thread that many ms. The
    /// no_std core has no clock, so the embedder sets these (the host from `std::time`; a device from
    /// a hardware timer + WFI). When unset, timed waits do not block -- a deadline is treated as
    /// already due, so wake ORDERING is right but there is no real delay.
    now_millis_fn: Option<fn() -> u64>,
    sleep_millis_fn: Option<fn(u64)>,
    /// The host networking seam ([`crate::net::NetBackend`]) -- non-blocking sockets + a readiness
    /// poll for the scheduler's reactor. The no_std core keeps none; the embedder sets it (the host
    /// with `std::net` + `mio`; a device with lwIP / an AT modem). `None` until set -- socket ops
    /// then report failure (no networking on this build).
    net_backend: Option<alloc::boxed::Box<dyn crate::net::NetBackend>>,
    /// The host TLS crypto seam ([`crate::tls::TlsBackend`]) -- a pure byte-transform (no sockets, no
    /// blocking) the managed `SslStream` drives over the socket layer. `None` until set (no TLS on
    /// this build); the embedder supplies it (the host with rustls / mbedTLS; a device with mbedTLS).
    tls_backend: Option<alloc::boxed::Box<dyn crate::tls::TlsBackend>>,
    /// The AEAD crypto seam ([`crate::aead::AeadBackend`]) -- RFC 5297 AES-SIV for the
    /// authenticated protocol tier (NTS). `None` until the embedder installs one; the AEAD
    /// intrinsics then report failure rather than encrypt with nothing.
    aead_backend: Option<alloc::boxed::Box<dyn crate::aead::AeadBackend>>,
    /// The file-system mount table ([`crate::mount::MountTable`]) -- the runtime VFS the `System.IO`
    /// intrinsics route through. Empty until a backend is mounted (a device with no file system
    /// mounts none): every managed file operation then reports [`crate::fs::FsError::Io`] and the
    /// corlib throws a catchable `IOException`. A device with one file system is a one-entry `/`
    /// table -- identical to holding a single backend.
    mounts: crate::mount::MountTable,
    /// The names of the file-system formats this build can create ([`Vm::set_filesystems`]) -- the
    /// managed `DriveInfo.GetFileSystems`. The embedder that links a formatter declares them (e.g.
    /// `["FAT"]`); empty by default (no format capability).
    filesystems: alloc::vec::Vec<alloc::string::String>,
    /// The program's own arguments ([`Vm::set_program_args`]), which [`entry_args`] gives a
    /// `Main(string[] args)` as its array. Empty unless the embedder sets them, so such a `Main`
    /// gets an empty array, as .NET gives a program started with no arguments.
    program_args: alloc::vec::Vec<alloc::string::String>,
    /// The embedder's on-demand mount-backend provider ([`crate::mount::StorageProvider`]) -- the FAT
    /// / SD / RAM constructor the managed `Storage.Mount*` routes through. `None` until installed; the
    /// mount calls then report NotSupported.
    storage_provider: Option<alloc::boxed::Box<dyn crate::mount::StorageProvider>>,
    /// The serial-port seam ([`crate::serial::SerialBackend`]) -- a blocking UART/COM byte pipe.
    /// `None` until set (a device with no serial hardware installs none): every managed
    /// `System.IO.Ports.SerialPort` operation then reports [`crate::serial::SerialError::Io`] and
    /// the corlib throws a catchable `IOException`.
    serial_backend: Option<alloc::boxed::Box<dyn crate::serial::SerialBackend>>,
    /// The live console tap ([`Vm::set_console_tap`]): a bench embedder mirrors every console
    /// write to its UART as it happens, so a run that never completes still narrates itself.
    console_tap: Option<fn(&[u16])>,
    /// The native off-heap memory seam ([`crate::memory::MemoryBackend`]) -- `Marshal.AllocHGlobal`
    /// + raw `Read*`/`Write*`. `None` until the embedder selects a backend (the Safe bounds-checked
    /// block table or Real host pointers); Marshal ops then report no native memory on this build.
    memory_backend: Option<alloc::boxed::Box<dyn crate::memory::MemoryBackend>>,
    /// The P/Invoke host seam: a `[DllImport]` dispatch resolves + calls the native entry
    /// through this hook. `None` (the device tiers, or a host that keeps interop off)
    /// makes such a call trap as unresolved.
    pinvoke_host: Option<PInvokeHostFn>,
    /// The volatile MMIO seam ([`Lamella.Hardware.Mmio`]) -- a C# peripheral driver's register
    /// access. The no_std core does no raw pointer I/O itself (it forbids `unsafe`); the embedder
    /// installs these (a device firmware with `write_volatile`/`read_volatile` at the real address;
    /// a host harness that wants real access). `None` falls back to the host simulated register
    /// file below, so a driver runs + is verifiable off-device with no seam installed.
    /// The embedder's REMAINING-HEADROOM probe: bytes the allocator can still hand out. A long-
    /// lived session (the device REPL) never reclaims, so one that runs long enough exhausts its
    /// arena -- and because Rust's allocation path is infallible, exhaustion ABORTS rather than
    /// erroring, which on a device is a panic, a reset, and a session that vanishes silently.
    /// With this installed, the session asks BEFORE it starts allocating and refuses cleanly
    /// instead. `None` (the host, where the heap is effectively unbounded) never refuses, so host
    /// behaviour is unchanged.
    heap_headroom_fn: Option<fn() -> usize>,
    mmio_write_fn: Option<fn(u32, u32)>,
    mmio_read_fn: Option<fn(u32) -> u32>,
    /// The SUB-WORD volatile MMIO seam (`Mmio` Read8/Write8/Read16/Write16). A driver needs these
    /// where a 32-bit access is impossible: Cortex-M0+ (ARMv6-M) faults on an unaligned word, and
    /// byte/halfword-addressed registers (SAMD pinmux, STM32 config bytes, ...) sit at unaligned
    /// addresses. A device installs them beside the 32-bit seam; `None` falls back to the host sim.
    mmio_write8_fn: Option<fn(u32, u8)>,
    mmio_read8_fn: Option<fn(u32) -> u8>,
    mmio_write16_fn: Option<fn(u32, u16)>,
    mmio_read16_fn: Option<fn(u32) -> u16>,
    /// A simulated MMIO register file, HOST-ONLY: the default `Mmio` target when no seam is
    /// installed, so a driver can be exercised and its register writes verified off-device. On a
    /// device build (`target_os = "none"`) this field does not exist -- MMIO with no seam is a
    /// no-op (the firmware is expected to install the real-register seam at boot).
    #[cfg(not(target_os = "none"))]
    mmio_sim: BTreeMap<u32, u32>,
}

impl Vm {
    /// Creates a fresh runtime context with an empty heap and no output.
    #[must_use]
    pub fn new() -> Vm {
        Vm::default()
    }

    /// Requests a collection at the next safepoint (`GC.Collect`).
    #[cfg(feature = "gc")]
    pub fn request_collect(&mut self) {
        self.force_collect = true;
        self.heap.note_collection_check();
    }

    /// Takes and clears any pending forced-collection request.
    #[cfg(feature = "gc")]
    pub fn take_force_collect(&mut self) -> bool {
        core::mem::take(&mut self.force_collect)
    }

    /// Suspends collection until the matching [`Vm::resume_collection`] (nestable). A
    /// reflective intrinsic that constructs objects across a nested interpreter run holds this
    /// so an in-progress instance it roots only in a Rust local cannot be relocated/reclaimed by
    /// a GC the nested run triggers.
    #[cfg(feature = "gc")]
    pub fn suspend_collection(&mut self) {
        self.collect_suspend = self.collect_suspend.saturating_add(1);
    }

    /// Ends one [`Vm::suspend_collection`]. Collection resumes once every suspension is lifted.
    #[cfg(feature = "gc")]
    pub fn resume_collection(&mut self) {
        self.collect_suspend = self.collect_suspend.saturating_sub(1);
        self.heap.note_collection_check();
    }

    /// Whether collection is currently suspended (a non-zero suspension depth).
    #[cfg(feature = "gc")]
    #[must_use]
    pub fn collection_suspended(&self) -> bool {
        self.collect_suspend != 0
    }

    /// Parks a caller's `frames` for the duration of a nested run, so a collection inside that run
    /// marks and REMAPS them instead of leaving them stale ([`ParkedFrames`]).
    ///
    /// Must be balanced by [`Vm::unpark_frames`] on every path out, the error path included: the
    /// caller's frame vector is left empty until it is taken back, and a session with no frames
    /// reports itself finished.
    #[cfg(feature = "gc")]
    fn park_frames(&mut self, frames: Vec<Frame>) {
        self.parked.activations.push(frames);
    }

    /// Takes back the frames from the matching [`Vm::park_frames`], relocated if a collection ran.
    #[cfg(feature = "gc")]
    fn unpark_frames(&mut self) -> Vec<Frame> {
        self.parked.activations.pop().unwrap_or_default()
    }

    /// The managed heap.
    #[must_use]
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// The managed heap, mutably (to allocate objects).
    pub fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// Sets the HARD live-object budget (`None` = unbounded, the host default). A device on a
    /// fixed heap sets it below the real capacity so an exhausting program raises a catchable
    /// `OutOfMemoryException` instead of failing the allocator hard. See [`Heap::at_budget`].
    pub fn set_object_budget(&mut self, budget: Option<usize>) {
        self.heap.set_object_budget(budget);
    }

    /// Sets a fixed collection interval, in allocations, in place of the adaptive threshold
    /// (`None`, the default, restores it). `Some(1)` collects at the instruction boundary after
    /// every allocation, the stress setting for testing the collector's roots. See
    /// [`Heap::set_collection_interval`].
    #[cfg(feature = "gc")]
    pub fn set_collection_interval(&mut self, interval: Option<core::num::NonZeroUsize>) {
        self.heap.set_collection_interval(interval);
    }

    /// Makes every collection move every object that survives it (`false`, the default, compacts
    /// in place), so a reference the collector cannot see is wrong after any collection rather
    /// than only after one that reclaimed something below its object. The other stress setting,
    /// beside [`Vm::set_collection_interval`]. See [`Heap::set_move_every_survivor`].
    #[cfg(feature = "gc")]
    pub fn set_move_every_survivor(&mut self, move_every_survivor: bool) {
        self.heap.set_move_every_survivor(move_every_survivor);
    }

    /// Reserves the next green-thread id (`Thread.Start`). Ids start at 1; thread 0 is main.
    pub fn alloc_thread_id(&mut self) -> u32 {
        if self.next_thread_id == 0 {
            self.next_thread_id = 1;
        }
        let id = self.next_thread_id;
        self.next_thread_id += 1;
        id
    }

    /// Requests that the scheduler spawn a green thread (under the reserved `id`) running `method`
    /// with `target` as its receiver (`Null` for a static method). Raised by `Thread.Start`.
    pub fn request_spawn(&mut self, id: u32, method: MethodId, target: Value, background: bool) {
        self.thread_op = Some(ThreadOp::Spawn { id, method, target, background });
    }

    /// Requests a cooperative yield to the scheduler (`Thread.Yield` / `Thread.Sleep`).
    pub fn request_yield(&mut self) {
        self.thread_op = Some(ThreadOp::Yield);
    }

    /// Requests that the running thread block until thread `target` completes (`Thread.Join`).
    pub fn request_join(&mut self, target: u32, deadline: Option<u64>) {
        self.thread_op = Some(ThreadOp::Join { target, deadline });
    }

    /// Records that `thread`'s current timed park ran out of time, for the managed side to read back
    /// through [`take_wait_timed_out`](Self::take_wait_timed_out) once it runs again.
    ///
    /// Separate from [`resolve_lock_deadline`](Self::resolve_lock_deadline), which records the same
    /// verdict as one outcome of a decision it also has to make; a join has no such decision.
    pub fn record_park_timeout(&mut self, thread: u32) {
        self.wait_timeouts.insert(thread);
    }

    /// The id of the green thread currently running.
    #[must_use]
    pub fn current_thread_id(&self) -> u32 {
        self.current_thread_id
    }

    fn set_current_thread(&mut self, id: u32) {
        self.current_thread_id = id;
    }

    fn take_thread_op(&mut self) -> Option<ThreadOp> {
        self.thread_op.take()
    }

    fn has_thread_op(&self) -> bool {
        self.thread_op.is_some()
    }

    /// The scheduler maintains the LIVE green-thread count so a GC safepoint can tell whether an
    /// inline collection (one thread's roots) is safe or must be deferred to an all-threads collect.
    fn set_live_thread_count(&mut self, count: u32) {
        self.live_thread_count = count;
    }

    fn live_thread_count(&self) -> u32 {
        self.live_thread_count
    }

    /// Interns a resolved Type reference while retaining a real handle for metadata lookup.
    pub(crate) fn reflection_type_handle(&mut self, identity: CastElem, handle: i64) -> i64 {
        *self.reflection_type_handles.entry(identity).or_insert(handle)
    }

    /// The private monitor object for a canonical reflection identity. The caller resolves
    /// cross-assembly aliases before looking it up; collection relocates the cached reference.
    pub(crate) fn reflection_monitor(&mut self, handle: CastElem) -> ObjectRef {
        if let Some(&object) = self.reflection_monitors.get(&handle) {
            return object;
        }
        let object = self.heap.alloc_array(Vec::new());
        self.reflection_monitors.insert(handle, object);
        object
    }

    /// Acquires the `Monitor` lock on object `obj` (its heap index) for `thread`. Returns `true` if
    /// the lock is now held -- the object was free (a fresh entry is created) or `thread` already
    /// owned it (a recursive `lock` bumps the depth) -- so the caller proceeds into the critical
    /// section. Returns `false` on contention: `thread` is appended to the lock's wait queue (once)
    /// and the caller must block ([`Vm::request_block_on_lock`]); it is handed the lock, and woken,
    /// when the owner releases.
    pub fn lock_acquire(&mut self, obj: u32, thread: u32) -> bool {
        if self.lock_try(obj, thread) {
            return true;
        }
        if let Some(state) = self.locks.get_mut(&obj) {
            if !state.waiters.iter().any(|waiter| waiter.thread == thread) {
                state.waiters.push(LockWaiter { thread, depth: 1, abandonable: false });
            }
        }
        false
    }

    /// Releases one level of `thread`'s `Monitor` lock on `obj`. Returns the id of a newly-woken
    /// waiter when the outermost level is released and a thread is queued: the lock is HANDED to the
    /// first waiter (its `owner`/`count` are set so it resumes already holding the lock), whose id is
    /// returned for [`Vm::request_wake`]. Returns `None` when the lock is still held recursively, when
    /// it falls idle (the entry is dropped unless `Monitor.Wait`-ers keep it alive), or when `thread`
    /// is not the owner.
    pub fn lock_release(&mut self, obj: u32, thread: u32) -> Option<u32> {
        let state = self.locks.get_mut(&obj)?;
        if state.owner != Some(thread) {
            return None;
        }
        state.count -= 1;
        if state.count > 0 {
            return None;
        }
        if let Some(next) = state.grant_to_next_waiter() {
            return Some(next);
        }
        if state.waiting.is_empty() {
            self.locks.remove(&obj);
        }
        None
    }

    /// Tries to acquire the `Monitor` lock on `obj` for `thread` WITHOUT blocking (`Monitor.TryEnter`
    /// and the primitive [`Vm::lock_acquire`] builds on): `true` for a free object (no entry, or one
    /// left unowned by a `Monitor.Wait`) or a recursive acquire (the depth bumps); `false` on
    /// contention, with NO enqueue.
    pub fn lock_try(&mut self, obj: u32, thread: u32) -> bool {
        match self.locks.get_mut(&obj) {
            None => {
                self.locks.insert(obj, LockState::owned_by(thread));
                true
            }
            Some(state) if state.owner.is_none() => {
                state.owner = Some(thread);
                state.count = 1;
                true
            }
            Some(state) if state.owner == Some(thread) => {
                state.count += 1;
                true
            }
            Some(_) => false,
        }
    }

    /// Whether `thread` owns the `Monitor` lock on `obj` -- the precondition for `Monitor.Wait` /
    /// `Pulse` / `PulseAll` (which raise `SynchronizationLockException` otherwise).
    #[must_use]
    pub fn lock_is_owner(&self, obj: u32, thread: u32) -> bool {
        matches!(self.locks.get(&obj), Some(state) if state.owner == Some(thread))
    }

    /// `Monitor.Wait`: the owning `thread` parks in the lock's condition wait-set (at its current
    /// recursion depth, to restore on reacquire) and FULLY releases the lock. Returns the id of an
    /// acquire-waiter handed the lock by this release (to wake), or `None`. The caller must have
    /// verified ownership ([`Vm::lock_is_owner`]) and then blocks the thread.
    pub fn lock_wait(&mut self, obj: u32, thread: u32) -> Option<u32> {
        let state = self.locks.get_mut(&obj)?;
        let depth = state.count;
        state.waiting.push(LockWaiter { thread, depth, abandonable: false });
        state.grant_to_next_waiter()
    }

    /// `Monitor.Pulse` (`one` = true) / `Monitor.PulseAll` (false): moves the first / all threads
    /// blocked in `Monitor.Wait` on `obj` into the acquire-queue, preserving FIFO order. They do not
    /// run yet -- the first is handed the lock when the (still-owning) pulser later releases it. The
    /// caller must have verified ownership.
    pub fn lock_pulse(&mut self, obj: u32, one: bool) {
        let Some(state) = self.locks.get_mut(&obj) else {
            return;
        };
        if one {
            if !state.waiting.is_empty() {
                let waiter = state.waiting.remove(0);
                state.waiters.push(waiter);
            }
        } else {
            let pulsed = core::mem::take(&mut state.waiting);
            state.waiters.extend(pulsed);
        }
    }

    /// Requests that the scheduler block the running thread on a `Monitor` lock -- either a failed
    /// acquire (it is queued in the lock's waiters) or a `Monitor.Wait` (it is parked in the
    /// condition wait-set, having released the lock). The thread is parked until a later wake; by
    /// then the lock has been handed to it, so it resumes already holding it. `wake`, if set, is a
    /// contender that a `Monitor.Wait`'s release just handed the lock -- the scheduler wakes it too.
    pub fn request_block_on_lock(&mut self, wake: Option<u32>) {
        self.thread_op = Some(ThreadOp::BlockOnLock { wake, deadline: None });
    }

    /// As [`request_block_on_lock`](Self::request_block_on_lock), but the park is BOUNDED: the
    /// reactor also wakes the thread once `deadline` (monotonic ms) passes, whether or not it was
    /// pulsed. Backs `Monitor.Wait(object, int)`.
    pub fn request_block_on_lock_until(&mut self, wake: Option<u32>, deadline: u64) {
        self.thread_op = Some(ThreadOp::BlockOnLock { wake, deadline: Some(deadline) });
    }

    /// Queue `thread` for a TIMED acquire of `obj`'s lock -- the blocking half of
    /// `Monitor.TryEnter(object, int)`. Returns `true` if it was queued (so the caller parks);
    /// `false` if the lock proves takeable and is now HELD (so the caller must not park).
    ///
    /// The second case is defensive rather than expected: the managed side tries first and only
    /// calls this on contention, and nothing runs between the two on a cooperative scheduler. But a
    /// park with no one to wake it is a hang, so the takeable case is handled rather than assumed
    /// away.
    pub fn lock_queue_timed_acquire(&mut self, obj: u32, thread: u32) -> bool {
        if self.lock_try(obj, thread) {
            return false;
        }
        if let Some(state) = self.locks.get_mut(&obj) {
            if !state.waiters.iter().any(|waiter| waiter.thread == thread) {
                state.waiters.push(LockWaiter { thread, depth: 1, abandonable: true });
            }
        }
        true
    }

    /// A deadline fired on a thread parked against a lock: decide what actually happened and leave
    /// it correctly queued. Returns `true` when the thread may run now, `false` when it must stay
    /// blocked for a hand-off.
    ///
    /// THE RACE THIS EXISTS TO GET RIGHT: a hand-off and the deadline can land in the same block
    /// point, and the two live in the SAME acquire-queue with opposite obligations. So the answer
    /// comes from WHICH queue the thread is in and, within the acquire-queue, from whether it is
    /// allowed to abandon:
    ///
    /// * still in the condition WAIT-SET -- a timed `Monitor.Wait` that was never pulsed. A real
    ///   timeout, and it does NOT skip reacquisition: .NET returns from `Wait` holding the lock
    ///   either way, so it takes the lock if free and otherwise queues like a contended `Enter`.
    /// * in the acquire-queue and ABANDONABLE -- a timed `TryEnter` that ran out of time. A real
    ///   timeout, and it leaves the queue: its answer is "no", and staying would hand it a lock
    ///   nobody is waiting to receive.
    /// * in the acquire-queue and NOT abandonable -- a `Wait` a pulse already moved there, which is
    ///   owed the lock. NOT a timeout; waking it here would return it without the lock.
    /// * in neither -- it already holds the lock (a hand-off got there first). Runnable, and not a
    ///   timeout.
    pub fn resolve_lock_deadline(&mut self, thread: u32) -> bool {
        for (_, state) in self.locks.iter_mut() {
            if let Some(index) = state.waiting.iter().position(|w| w.thread == thread) {
                let waiter = state.waiting.remove(index);
                self.wait_timeouts.insert(thread);
                return if state.owner.is_none() {
                    state.owner = Some(waiter.thread);
                    state.count = waiter.depth;
                    true
                } else {
                    state.waiters.push(waiter);
                    false
                };
            }
            if let Some(index) = state.waiters.iter().position(|w| w.thread == thread) {
                if !state.waiters[index].abandonable {
                    return false;
                }
                state.waiters.remove(index);
                self.wait_timeouts.insert(thread);
                return true;
            }
        }
        true
    }

    /// Whether `thread`'s most recent TIMED PARK ended by its deadline, CONSUMING the verdict so a
    /// later park cannot read a stale one.
    ///
    /// One verdict serves all three timed parks -- `Monitor.Wait(object, int)`, a timed
    /// `Monitor.TryEnter`, and `Thread.Join(int)` -- because they ask one question, "did I get it in
    /// time", and a thread is parked in exactly one of them at a time. A second flag per park would
    /// be a second answer to that question, and the two would drift.
    pub fn take_wait_timed_out(&mut self, thread: u32) -> bool {
        self.wait_timeouts.remove(&thread)
    }

    /// Records that `thread` has run to completion, for `Thread.IsAlive` to read back.
    ///
    /// Called from the ONE site that sets `ThreadState::Done`, which is what keeps this a single
    /// writer rather than a mirror of the scheduler's state.
    pub fn mark_thread_finished(&mut self, thread: u32) {
        self.finished_threads.insert(thread);
    }

    /// Whether `thread` has run to completion. Not consuming: a finished thread stays finished.
    #[must_use]
    pub fn is_thread_finished(&self, thread: u32) -> bool {
        self.finished_threads.contains(&thread)
    }

    /// Requests that the scheduler wake thread `id`, blocked on a `Monitor` lock that a release
    /// (`Monitor.Exit`) just handed to it.
    pub fn request_wake(&mut self, id: u32) {
        self.thread_op = Some(ThreadOp::WakeThread { id });
    }

    /// Requests that the scheduler block the running thread for `millis` milliseconds (`Thread.Sleep`):
    /// other threads run meanwhile, and the scheduler idle-sleeps the OS thread to the nearest deadline.
    pub fn request_sleep(&mut self, millis: u64) {
        self.thread_op = Some(ThreadOp::SleepFor(millis));
    }

    /// Requests that the scheduler park the running thread for good (`Thread.Sleep(Timeout.Infinite)`):
    /// nothing wakes it, and the program keeps running its other threads, timers and pin events.
    pub fn request_sleep_forever(&mut self) {
        self.thread_op = Some(ThreadOp::SleepForever);
    }

    /// Sets the host clock seam: a monotonic-millisecond reader and an OS-thread sleep, used by the
    /// scheduler's idle-block and `Thread.Sleep`. The embedder provides these (the host from
    /// `std::time`); the no_std core keeps no clock of its own.
    pub fn set_clock(&mut self, now_millis: fn() -> u64, sleep_millis: fn(u64)) {
        self.now_millis_fn = Some(now_millis);
        self.sleep_millis_fn = Some(sleep_millis);
    }

    /// The current monotonic millisecond count, if a clock seam is set.
    #[must_use]
    pub fn now_millis(&self) -> Option<u64> {
        self.now_millis_fn.map(|now| now())
    }

    /// Blocks the OS thread for `millis` milliseconds through the clock seam ([`Vm::set_clock`]); a
    /// no-op without one. A network backend whose link needs the processor's time -- a radio it
    /// drives by polling -- spends the wait servicing it instead ([`crate::net::NetBackend::idle`]).
    ///
    /// It can return early: a host whose thread may not block, such as a browser's main thread,
    /// returns at once. So a caller waiting for a deadline asks [`Vm::now_millis`] again after each
    /// call rather than taking one call to have lasted `millis`.
    pub fn sleep_millis(&mut self, millis: u64) {
        if let Some(backend) = self.net_backend.as_deref_mut() {
            if backend.idle(millis) {
                return;
            }
        }
        if let Some(sleep) = self.sleep_millis_fn {
            sleep(millis);
        }
    }

    /// Sets the host networking seam ([`crate::net::NetBackend`]). The embedder provides it (the host
    /// with `std::net` + `mio`); without it, socket operations report failure.
    pub fn set_net_backend(&mut self, backend: alloc::boxed::Box<dyn crate::net::NetBackend>) {
        self.net_backend = Some(backend);
    }

    /// The host networking seam, if set -- the socket intrinsics and the reactor's idle poll use it.
    pub fn net_backend(&mut self) -> Option<&mut (dyn crate::net::NetBackend + 'static)> {
        self.net_backend.as_deref_mut()
    }

    /// The heap and the networking seam together, split-borrowed -- a bulk socket intrinsic
    /// crosses the seam straight to/from a packed array's bytes, which needs a view into the
    /// heap live while the backend runs.
    pub fn heap_and_net(
        &mut self,
    ) -> (&mut Heap, Option<&mut (dyn crate::net::NetBackend + 'static)>) {
        (&mut self.heap, self.net_backend.as_deref_mut())
    }

    /// Sets the host TLS crypto seam ([`crate::tls::TlsBackend`]). The embedder provides it (the host
    /// with rustls / mbedTLS; a device with mbedTLS); without it, the TLS intrinsics report failure.
    pub fn set_tls_backend(&mut self, backend: alloc::boxed::Box<dyn crate::tls::TlsBackend>) {
        self.tls_backend = Some(backend);
    }

    /// The host TLS crypto seam, if set -- the `System.Net.Security` intrinsics drive it. Pure: it
    /// transforms buffers only, so (unlike the socket seam) it is never involved in the reactor.
    pub fn tls_backend(&mut self) -> Option<&mut (dyn crate::tls::TlsBackend + 'static)> {
        self.tls_backend.as_deref_mut()
    }

    /// Sets the AEAD crypto seam ([`crate::aead::AeadBackend`]) -- RFC 5297 AES-SIV. The
    /// embedder provides it (host: RustCrypto aes-siv; device: the mbedTLS-composed SIV);
    /// without it the AEAD intrinsics report failure.
    pub fn set_aead_backend(&mut self, backend: alloc::boxed::Box<dyn crate::aead::AeadBackend>) {
        self.aead_backend = Some(backend);
    }

    /// The AEAD crypto seam, if set. Pure byte transforms, like the TLS seam.
    pub fn aead_backend(&mut self) -> Option<&mut (dyn crate::aead::AeadBackend + 'static)> {
        self.aead_backend.as_deref_mut()
    }

    /// Mounts the embedder's file-system backend ([`crate::fs::FsBackend`]) at the ROOT (`/`) -- the
    /// single-file-system case (the host with `std::fs`; a device with a FAT-over-SD driver; tests
    /// with the in-memory tree). Behavior is identical to holding one backend; [`Vm::mount`] adds
    /// further file systems at other prefixes. Without any mount, every managed file operation
    /// throws a catchable `IOException`.
    pub fn set_fs_backend(&mut self, backend: alloc::boxed::Box<dyn crate::fs::FsBackend>) {
        let _ = self.mounts.mount("/", backend);
    }

    /// Mounts `backend` at `prefix` (`"/mnt/sd0"`, `"D:"`, ...) -- a second file system beside the root.
    ///
    /// # Errors
    /// [`crate::fs::FsError::InvalidPath`] if `prefix` escapes the root (a leading `..`).
    pub fn mount(
        &mut self,
        prefix: &str,
        backend: alloc::boxed::Box<dyn crate::fs::FsBackend>,
    ) -> crate::fs::FsResult<()> {
        self.mounts.mount(prefix, backend)
    }

    /// Unmounts the file system at `prefix` exactly, returning whether one was removed. Handles
    /// still open on it go stale (their next use reports [`crate::fs::FsError::Io`]).
    ///
    /// # Errors
    /// [`crate::fs::FsError::InvalidPath`] if `prefix` escapes the root.
    pub fn unmount(&mut self, prefix: &str) -> crate::fs::FsResult<bool> {
        self.mounts.unmount(prefix)
    }

    /// The file-system mount table -- the `System.IO` intrinsics drive it. Blocking by design
    /// (bounded operations, no reactor involvement -- see the [`crate::fs`] module docs).
    pub fn mounts(&mut self) -> &mut crate::mount::MountTable {
        &mut self.mounts
    }

    /// The heap and the mount table together, split-borrowed -- a bulk read/write intrinsic crosses
    /// the seam straight to/from a packed array's bytes, which needs a view into the heap live while
    /// the backend runs.
    pub fn heap_and_mounts(&mut self) -> (&mut Heap, &mut crate::mount::MountTable) {
        (&mut self.heap, &mut self.mounts)
    }

    /// Declares the file-system format names this build can create (for `DriveInfo.GetFileSystems`).
    /// The embedder that links a formatter sets them, e.g. `vm.set_filesystems(&["FAT"])`.
    pub fn set_filesystems(&mut self, names: &[&str]) {
        self.filesystems = names
            .iter()
            .map(|name| alloc::string::String::from(*name))
            .collect();
    }

    /// The declared file-system format names ([`Vm::set_filesystems`]).
    #[must_use]
    pub fn filesystems(&self) -> &[alloc::string::String] {
        &self.filesystems
    }

    /// Sets the program's own arguments, which [`entry_args`] gives a `Main(string[] args)`: what
    /// follows `--` on a `dotnet run` or `lamella run` command line.
    pub fn set_program_args(&mut self, args: alloc::vec::Vec<alloc::string::String>) {
        self.program_args = args;
    }

    /// The program's own arguments ([`Vm::set_program_args`]).
    #[must_use]
    pub fn program_args(&self) -> &[alloc::string::String] {
        &self.program_args
    }

    /// Installs the embedder's on-demand mount-backend provider (the FAT / SD / RAM constructor).
    /// Without one, every managed `Storage.Mount*` reports [`crate::fs::FsError::Unsupported`].
    pub fn set_storage_provider(
        &mut self,
        provider: alloc::boxed::Box<dyn crate::mount::StorageProvider>,
    ) {
        self.storage_provider = Some(provider);
    }

    /// Mounts a fresh RAM-backed volume of `size_bytes` at `prefix` (the managed `Storage.MountRam`):
    /// the embedder's provider constructs the backend, which is mounted as
    /// [`crate::mount::DriveType::Ram`].
    ///
    /// # Errors
    /// [`crate::fs::FsError::Unsupported`] if no provider is installed or it does not support a RAM
    /// disk; [`crate::fs::FsError::InvalidPath`] if `prefix` escapes the root; or any error the
    /// provider raises constructing/formatting the volume.
    pub fn mount_ram(&mut self, prefix: &str, size_bytes: u64) -> crate::fs::FsResult<()> {
        let backend = self
            .storage_provider
            .as_mut()
            .ok_or(crate::fs::FsError::Unsupported)?
            .mount_ram(size_bytes)?;
        self.mounts
            .mount_as(prefix, backend, crate::mount::DriveType::Ram)
    }

    /// Mounts the FAT volume on an SD card wired to the SPI bus `bus_identity` names, with
    /// `chip_select` as the software chip-select pin, at `prefix` (the managed
    /// `Storage.MountSdOverSpi`). Mounted as [`crate::mount::DriveType::Removable`], which is what a
    /// card slot IS -- the managed `DriveInfo` then reports it as removable without being told.
    ///
    /// `bus_identity` is carried, never dereferenced: see
    /// [`crate::mount::StorageProvider::mount_sd_over_spi`] for what it names and why the two halves
    /// can agree on it without a bus-naming scheme. **Zero names no bus** and is refused here rather
    /// than passed on -- a provider should never be asked about the absence of a peripheral, and the
    /// managed tier spells "this driver has no native instance" exactly that way.
    ///
    /// # Errors
    /// [`crate::fs::FsError::Unsupported`] if `bus_identity` is 0, if no provider is installed, if
    /// the provider does not support SD-over-SPI, or if it recognizes no bus by that identity;
    /// [`crate::fs::FsError::InvalidPath`] if `prefix` escapes the root; or any error the provider
    /// raises bringing the card up (no card present, an unreadable partition, a volume that is not
    /// FAT).
    pub fn mount_sd_over_spi(
        &mut self,
        prefix: &str,
        bus_identity: u32,
        chip_select: i32,
    ) -> crate::fs::FsResult<()> {
        if bus_identity == 0 {
            return Err(crate::fs::FsError::Unsupported);
        }
        let backend = self
            .storage_provider
            .as_mut()
            .ok_or(crate::fs::FsError::Unsupported)?
            .mount_sd_over_spi(bus_identity, chip_select)?;
        self.mounts
            .mount_as(prefix, backend, crate::mount::DriveType::Removable)
    }

    /// Mounts the SD card on the SPI bus `bus_number` names, at `prefix` -- the nanoFramework-shaped
    /// `SDCard.Mount()`, whose `spiBus` is a bus NUMBER rather than the register base
    /// [`Vm::mount_sd_over_spi`] carries. Mounted as [`crate::mount::DriveType::Removable`].
    ///
    /// The two spellings stay apart all the way down to the provider; see
    /// [`crate::mount::StorageProvider::mount_sd_over_spi_bus`] for why conflating them would mount
    /// the wrong bus rather than fail.
    ///
    /// # Errors
    /// [`crate::fs::FsError::Unsupported`] if no provider is installed, the provider does not
    /// support naming a bus this way, or it knows no bus by that number;
    /// [`crate::fs::FsError::InvalidPath`] if `prefix` escapes the root; or any error the provider
    /// raises bringing the card up.
    pub fn mount_sd_over_spi_bus(
        &mut self,
        prefix: &str,
        bus_number: u32,
        chip_select: i32,
    ) -> crate::fs::FsResult<()> {
        let backend = self
            .storage_provider
            .as_mut()
            .ok_or(crate::fs::FsError::Unsupported)?
            .mount_sd_over_spi_bus(bus_number, chip_select)?;
        self.mounts
            .mount_as(prefix, backend, crate::mount::DriveType::Removable)
    }

    /// Sets the serial-port seam ([`crate::serial::SerialBackend`]). The embedder provides it (the
    /// host with the operating system's serial ports; a device with a SERCOM/USART driver;
    /// tests with the in-memory loopback); without it, every managed serial operation throws a
    /// catchable `IOException`.
    pub fn set_serial_backend(
        &mut self,
        backend: alloc::boxed::Box<dyn crate::serial::SerialBackend>,
    ) {
        self.serial_backend = Some(backend);
    }

    /// The serial-port seam, if set -- the `System.IO.Ports` intrinsics drive it. Blocking by
    /// design (bounded, timeout-capped operations, no reactor involvement -- see the
    /// [`crate::serial`] module docs).
    pub fn serial_backend(&mut self) -> Option<&mut (dyn crate::serial::SerialBackend + 'static)> {
        self.serial_backend.as_deref_mut()
    }

    /// The heap and the serial-port seam together, split-borrowed -- the bulk read/write
    /// intrinsics cross the seam straight to/from a packed array's bytes, which needs a view into
    /// the heap live while the backend runs (the same discipline as [`Vm::heap_and_mounts`]).
    pub fn heap_and_serial(
        &mut self,
    ) -> (&mut Heap, Option<&mut (dyn crate::serial::SerialBackend + 'static)>) {
        (&mut self.heap, self.serial_backend.as_deref_mut())
    }

    /// Sets the native off-heap memory seam ([`crate::memory::MemoryBackend`]). The embedder selects
    /// the mode at startup -- the Safe bounds-checked block table, or (Stage 2) Real host pointers;
    /// without it, the `Marshal` intrinsics report no native memory on this build.
    pub fn set_memory_backend(
        &mut self,
        backend: alloc::boxed::Box<dyn crate::memory::MemoryBackend>,
    ) {
        self.memory_backend = Some(backend);
    }

    /// The native off-heap memory seam, if set -- the `System.Runtime.InteropServices.Marshal`
    /// intrinsics drive it.
    pub fn memory_backend(&mut self) -> Option<&mut (dyn crate::memory::MemoryBackend + 'static)> {
        self.memory_backend.as_deref_mut()
    }

    /// Installs the P/Invoke host seam: the native-call hook a `[DllImport]` dispatch
    /// drives (a HOST runner resolves the library + entry and performs the foreign call;
    /// a device build leaves this unset and the call traps as unresolved).
    pub fn set_pinvoke_host(&mut self, host: PInvokeHostFn) {
        self.pinvoke_host = Some(host);
    }

    /// The P/Invoke host seam, if installed.
    #[must_use]
    pub fn pinvoke_host(&self) -> Option<PInvokeHostFn> {
        self.pinvoke_host
    }

    /// Installs the volatile MMIO seam (a device firmware supplies `write_volatile`/`read_volatile`
    /// at the real register address; the no_std core does no raw pointer I/O itself). Once set,
    /// `Mmio.Write32`/`Read32` route here instead of the host simulated register file.
    pub fn set_mmio(&mut self, write: fn(u32, u32), read: fn(u32) -> u32) {
        self.mmio_write_fn = Some(write);
        self.mmio_read_fn = Some(read);
    }

    /// Installs the embedder's remaining-headroom probe (see `heap_headroom_fn`): bytes the
    /// allocator can still hand out. A device firmware installs its arena's; a host leaves it unset.
    pub fn set_heap_headroom(&mut self, probe: fn() -> usize) {
        self.heap_headroom_fn = Some(probe);
    }

    /// Bytes the embedder's allocator can still hand out, or `None` when no probe is installed
    /// (a host, where the heap is effectively unbounded and nothing should be refused).
    #[must_use]
    pub fn heap_headroom(&self) -> Option<usize> {
        self.heap_headroom_fn.map(|probe| probe())
    }

    /// Installs the SUB-WORD volatile MMIO seam (8- and 16-bit `write_volatile`/`read_volatile`),
    /// the narrow counterpart of [`Vm::set_mmio`]. A device firmware installs both seams; the host
    /// leaves these unset and uses the aligned-word read-modify-write of the simulated register file.
    pub fn set_mmio_subword(
        &mut self,
        write8: fn(u32, u8),
        read8: fn(u32) -> u8,
        write16: fn(u32, u16),
        read16: fn(u32) -> u16,
    ) {
        self.mmio_write8_fn = Some(write8);
        self.mmio_read8_fn = Some(read8);
        self.mmio_write16_fn = Some(write16);
        self.mmio_read16_fn = Some(read16);
    }

    /// A `Mmio.Write32`: the installed volatile seam if any, else the HOST simulated register file
    /// (a no-op on a device build with no seam installed).
    pub fn mmio_write(&mut self, address: u32, value: u32) {
        if let Some(write) = self.mmio_write_fn {
            write(address, value);
            return;
        }
        #[cfg(not(target_os = "none"))]
        {
            self.mmio_sim.insert(address, value);
        }
    }

    /// A `Mmio.Read32`: the installed volatile seam if any, else the HOST simulated register file
    /// (0 on a device build with no seam installed).
    #[must_use]
    pub fn mmio_read(&self, address: u32) -> u32 {
        if let Some(read) = self.mmio_read_fn {
            return read(address);
        }
        #[cfg(not(target_os = "none"))]
        {
            return self.mmio_sim.get(&address).copied().unwrap_or(0);
        }
        #[cfg(target_os = "none")]
        0
    }

    /// A `Mmio.Write8`: the installed sub-word seam if any, else the HOST simulated register file as
    /// a read-modify-write of the aligned 32-bit word (a no-op on a device build with no seam). The
    /// sim keys every access to the aligned word, so 8-/16-/32-bit accesses to the same word stay
    /// coherent -- exactly as the silicon bus does.
    pub fn mmio_write8(&mut self, address: u32, value: u8) {
        if let Some(write) = self.mmio_write8_fn {
            write(address, value);
            return;
        }
        #[cfg(not(target_os = "none"))]
        {
            let word_addr = address & !0b11;
            let shift = (address & 0b11) * 8;
            let word = self.mmio_sim.get(&word_addr).copied().unwrap_or(0);
            self.mmio_sim
                .insert(word_addr, (word & !(0xFFu32 << shift)) | (u32::from(value) << shift));
        }
    }

    /// A `Mmio.Read8`: the installed sub-word seam if any, else the aligned-word HOST sim (0 on a
    /// device build with no seam installed).
    #[must_use]
    pub fn mmio_read8(&self, address: u32) -> u8 {
        if let Some(read) = self.mmio_read8_fn {
            return read(address);
        }
        #[cfg(not(target_os = "none"))]
        {
            let word = self.mmio_sim.get(&(address & !0b11)).copied().unwrap_or(0);
            (word >> ((address & 0b11) * 8)) as u8
        }
        #[cfg(target_os = "none")]
        0
    }

    /// A `Mmio.Write16`: the installed sub-word seam if any, else the aligned-word HOST sim RMW (a
    /// no-op on a device build with no seam). A 16-bit access is 2-byte aligned, so the halfword
    /// sits wholly within one word at bit offset 0 or 16.
    pub fn mmio_write16(&mut self, address: u32, value: u16) {
        if let Some(write) = self.mmio_write16_fn {
            write(address, value);
            return;
        }
        #[cfg(not(target_os = "none"))]
        {
            let word_addr = address & !0b11;
            let shift = (address & 0b10) * 8;
            let word = self.mmio_sim.get(&word_addr).copied().unwrap_or(0);
            self.mmio_sim
                .insert(word_addr, (word & !(0xFFFFu32 << shift)) | (u32::from(value) << shift));
        }
    }

    /// A `Mmio.Read16`: the installed sub-word seam if any, else the aligned-word HOST sim (0 on a
    /// device build with no seam installed).
    #[must_use]
    pub fn mmio_read16(&self, address: u32) -> u16 {
        if let Some(read) = self.mmio_read16_fn {
            return read(address);
        }
        #[cfg(not(target_os = "none"))]
        {
            let word = self.mmio_sim.get(&(address & !0b11)).copied().unwrap_or(0);
            (word >> ((address & 0b10) * 8)) as u16
        }
        #[cfg(target_os = "none")]
        0
    }

    /// The HOST simulated MMIO register file, for a test/harness to inspect what a driver wrote.
    #[cfg(not(target_os = "none"))]
    #[must_use]
    pub fn mmio_sim(&self) -> &BTreeMap<u32, u32> {
        &self.mmio_sim
    }

    /// Requests that the scheduler park the running thread on socket I/O (a socket op would block):
    /// register `socket` for `interest`, then wake when the reactor's poll reports it ready. A
    /// `Read` park on a handle with a receive timeout installed ([`Vm::set_recv_timeout`]) is also
    /// bounded by that deadline, so a silent peer cannot park the thread forever.
    pub fn request_block_on_io(&mut self, socket: u32, interest: crate::net::Interest) {
        let deadline = match interest {
            crate::net::Interest::Read => {
                self.socket_timeouts.deadline(socket, interest, self.now_millis())
            }
            crate::net::Interest::Write => None,
        };
        self.thread_op = Some(ThreadOp::BlockOnIo { socket, interest, deadline });
    }

    /// Installs socket `handle`'s receive timeout in milliseconds (the `Socket.ReceiveTimeout` /
    /// SO_RCVTIMEO shape); 0 clears it (infinite, the default). While set, each `Read`-interest
    /// park for the handle wakes by the deadline even if the socket never readies; the woken
    /// receive op re-polls, still sees WouldBlock, and the managed receive loop owns the expiry
    /// decision. Send-side parks are unaffected.
    pub fn set_recv_timeout(&mut self, handle: u32, millis: u32) {
        self.socket_timeouts.set(handle, crate::net::Interest::Read, millis);
    }

    /// Drop everything remembered about a socket handle, which a close must do: a backend may hand
    /// the same number out again, and a timeout outliving its socket becomes an unrelated socket's.
    pub fn forget_socket_timeouts(&mut self, handle: u32) {
        self.socket_timeouts.forget(handle);
    }

    /// The reactor's poll: block until a watched socket is ready or `timeout_ms` elapses, returning
    /// the ready handles (empty without a backend). Used by `idle_wait`.
    fn net_poll(&mut self, timeout_ms: Option<u64>) -> alloc::vec::Vec<u32> {
        match self.net_backend.as_deref_mut() {
            Some(backend) => backend.poll(timeout_ms),
            None => alloc::vec::Vec::new(),
        }
    }

    /// Drops a socket from the reactor's poll-set once the thread parked on it has been woken, so the
    /// poll-set holds only sockets with a currently-parked waiter (no stale-registration spurious wake).
    /// A later socket op that parks re-arms it via `register`. Used by `idle_wait`'s wake step.
    fn net_deregister(&mut self, socket: u32) {
        if let Some(backend) = self.net_backend.as_deref_mut() {
            backend.deregister(socket);
        }
    }

    /// Installs a LIVE console tap: every [`Vm::write`] also hands its units to `tap` as
    /// they arrive, before any cap clipping. A bench embedder points this at its UART so
    /// a run that never completes (and so never ships its buffered output) still streams
    /// its console in real time; `None` (the default) costs nothing.
    pub fn set_console_tap(&mut self, tap: fn(&[u16])) {
        self.console_tap = Some(tap);
    }

    /// Appends UTF-16 code units to the console output, clipping at the configured cap
    /// ([`Vm::set_output_cap`]) -- the units past it are dropped and
    /// [`Vm::output_clipped`] reports that it happened.
    pub fn write(&mut self, chars: &[u16]) {
        if let Some(tap) = self.console_tap {
            tap(chars);
        }
        let take = match self.output_cap {
            Some(cap) => cap.saturating_sub(self.output.len()).min(chars.len()),
            None => chars.len(),
        };
        if take < chars.len() {
            self.output_clipped = true;
        }
        self.output.extend_from_slice(&chars[..take]);
    }

    /// Bounds the console output buffer at `cap` UTF-16 units (`None` = unbounded, the
    /// host default). On a bump-allocator device tier an unbounded console is a leak an
    /// output-happy program feeds forever; a capped one is a fixed-cost diagnostic window
    /// (the embedder reads [`Vm::output_clipped`] to note the loss).
    ///
    /// For an embedder that discards what it has delivered ([`Vm::discard_output`]), the cap
    /// bounds what is waiting to be delivered, not everything the run has printed.
    pub fn set_output_cap(&mut self, cap: Option<usize>) {
        self.output_cap = cap;
    }

    /// Whether a [`Vm::write`] was clipped by the output cap since the last
    /// [`Vm::take_output_clipped`].
    #[must_use]
    pub fn output_clipped(&self) -> bool {
        self.output_clipped
    }

    /// Reads and clears the clipped flag (an embedder notes the truncation once per run).
    pub fn take_output_clipped(&mut self) -> bool {
        core::mem::take(&mut self.output_clipped)
    }

    /// The console output so far, as UTF-16 code units.
    ///
    /// Everything the run has printed, unless the embedder has discarded what it delivered
    /// ([`Vm::discard_output`]); then it is what has not been delivered yet.
    #[must_use]
    pub fn output(&self) -> &[u16] {
        &self.output
    }

    /// Forgets the first `units` UTF-16 units of the console output, which the embedder has
    /// delivered and will not read here again. More than there are forgets them all.
    ///
    /// This is for an embedder that STREAMS a run's output while it runs. Without it the buffer
    /// keeps every unit the program ever printed, so a program that prints for as long as it runs
    /// holds a copy of all of it -- on a device, until the memory runs out. With it, the buffer
    /// holds only what was printed since the last delivery.
    ///
    /// Nothing calls it for you. An embedder that never does sees [`Vm::output`] exactly as before.
    /// The buffer keeps its capacity, so a streaming run reuses the same storage instead of
    /// allocating it again.
    pub fn discard_output(&mut self, units: usize) {
        discard_front(&mut self.output, units);
    }

    /// Appends UTF-16 code units to the DEBUG channel (the `Debug.WriteLine` sink). Kept
    /// separate from [`Vm::write`] so the host can route it to STDERR, distinct from the
    /// Console's STDOUT.
    pub fn debug_write(&mut self, chars: &[u16]) {
        self.debug_output.extend_from_slice(chars);
    }

    /// The DEBUG-channel output so far, as UTF-16 code units -- or, for an embedder that discards
    /// what it delivered ([`Vm::discard_debug_output`]), what has not been delivered yet.
    #[must_use]
    pub fn debug_output(&self) -> &[u16] {
        &self.debug_output
    }

    /// [`Vm::discard_output`] for the DEBUG channel: forgets its first `units` UTF-16 units, which
    /// the embedder has delivered.
    pub fn discard_debug_output(&mut self, units: usize) {
        discard_front(&mut self.debug_output, units);
    }

    /// The DEBUG-channel output so far, decoded to a `String` (lossily, for display
    /// and tests).
    #[must_use]
    pub fn debug_output_string(&self) -> String {
        String::from_utf16_lossy(&self.debug_output)
    }

    /// The console output so far, decoded to a `String` (lossily, for display
    /// and tests).
    #[must_use]
    pub fn output_string(&self) -> String {
        String::from_utf16_lossy(&self.output)
    }

    /// How many static slots are live (the cheap half of the seeding guard).
    #[must_use]
    pub fn statics_len(&self) -> usize {
        self.statics.len()
    }

    /// Initializes the static-field storage from `defaults` on first use; idempotent
    /// once sized, so it never clobbers values written by `stsfld`.
    pub fn init_statics(&mut self, defaults: &[Value]) {
        if self.statics.len() < defaults.len() {
            self.statics = defaults.to_vec();
        }
    }

    /// Extends static storage to cover `defaults`, seeding ONLY the slots that did not exist
    /// before and leaving every existing value untouched.
    ///
    /// [`Vm::init_statics`] replaces the whole vector when it grows, which is right for sizing a
    /// fresh VM and wrong for a module that gains static fields while the VM is live -- an
    /// incremental session materializing a corlib member gains storage the running VM has not got,
    /// and a `stsfld` into a slot past the end is silently dropped.
    pub fn grow_statics(&mut self, defaults: &[Value]) {
        if defaults.len() > self.statics.len() {
            self.statics.extend_from_slice(&defaults[self.statics.len()..]);
        }
    }

    /// The not-yet-run `.cctor` of `type_id`, marking it run (II.10.5.3 lazy
    /// initialization). `Ok(None)` if the type has no cctor or it already ran in this VM.
    /// Marking happens HERE -- before the caller pushes the cctor's frame -- so a
    /// recursive trigger inside the cctor sees the type as initialized (the partial
    /// state the spec allows) and the replayed trigger instruction does not re-fire.
    ///
    /// # Errors
    /// [`Trap::TypeInitializationFailed`] when the type's initializer already THREW: the type
    /// stays inaccessible (II.10.5.3.1 guarantee 3) and this access must fail the way the first
    /// one did.
    ///
    pub(crate) fn take_pending_cctor(
        &mut self,
        module: &Module,
        type_id: TypeId,
    ) -> Result<Option<MethodId>, TypeId> {
        #[cfg(feature = "exceptions")]
        if self.cctors_failed.iter().any(|&(failed, _)| failed == type_id) {
            return Err(type_id);
        }
        let Some(cctor) = module.cctor_of_type(type_id) else {
            return Ok(None);
        };
        if self.cctors_run.insert(type_id) {
            Ok(Some(cctor))
        } else {
            Ok(None)
        }
    }

    /// The `TypeInitializationException` a failed `.cctor` left behind for `type_id`, if its
    /// initializer threw. Every trigger site asks this BEFORE
    /// [`take_pending_cctor`](Self::take_pending_cctor), because a failed type is also a RUN type
    /// and that call answers `None` for both -- which is exactly how the second access used to
    /// sail past into unassigned statics.
    #[cfg(feature = "exceptions")]
    #[must_use]
    pub(crate) fn cctor_failure(&self, type_id: TypeId) -> Option<ObjectRef> {
        self.cctors_failed
            .iter()
            .find(|&&(failed, _)| failed == type_id)
            .map(|&(_, exception)| exception)
    }

    /// Records that `type_id`'s initializer threw, and the exception to raise at every later
    /// access. Called as the cctor's frame unwinds, so the wrapper is minted once.
    #[cfg(feature = "exceptions")]
    fn note_cctor_failed(&mut self, type_id: TypeId, exception: ObjectRef) {
        self.cctors_failed.push((type_id, exception));
    }

    /// The type whose failed initializer produced `exception`, if it is such a wrapper -- the
    /// reverse of [`cctor_failure`](Self::cctor_failure), backing
    /// `TypeInitializationException.TypeName`.
    ///
    #[cfg(feature = "exceptions")]
    #[must_use]
    pub(crate) fn failed_type_of(&self, exception: ObjectRef) -> Option<TypeId> {
        self.cctors_failed
            .iter()
            .find(|&&(_, wrapper)| wrapper == exception)
            .map(|&(type_id, _)| type_id)
    }

    /// Marks every type's `.cctor` as already run -- the EAGER embedder's escape hatch, for an
    /// image baked before the trigger tables, whose cctors must run up front.
    pub fn mark_all_cctors_run(&mut self, module: &Module) {
        for &cctor in module.static_ctors() {
            if let Some(type_id) = module.method_type(cctor) {
                self.cctors_run.insert(type_id);
            }
        }
    }

    /// Whether every `.cctor` in `module`'s eager list has been marked run in this VM.
    ///
    /// The exact dual of [`Vm::mark_all_cctors_run`]: it reads what that writes, by the same
    /// lookup, so a module the marker has been run over always reports `true`. A cctor whose
    /// declaring type does not resolve counts as marked, because the marker skips it too --
    /// the pair cannot disagree, and no correctly-booted module can be refused by mistake.
    #[must_use]
    pub fn all_cctors_run(&self, module: &Module) -> bool {
        module.static_ctors().iter().all(|&cctor| {
            module
                .method_type(cctor)
                .is_none_or(|type_id| self.cctors_run.contains(&type_id))
        })
    }

    /// The value of static field `slot`, if storage holds it.
    #[must_use]
    pub fn static_field(&self, slot: usize) -> Option<Value> {
        self.statics.get(slot).cloned()
    }

    /// Stores `value` into static field `slot` (a no-op if out of range).
    pub fn set_static_field(&mut self, slot: usize, value: Value) {
        if let Some(target) = self.statics.get_mut(slot) {
            *target = value;
        }
    }

    /// Sets the current wall time, as 100-nanosecond ticks since the .NET epoch
    /// (0001-01-01 00:00:00), ANCHORING it to the current monotonic reading so the clock then
    /// advances with elapsed time.
    ///
    /// # The legacy path: an embedder wants [`set_wall_clock`] instead
    ///
    /// The wall clock's state is MANAGED -- `Lamella.Runtime.Clock` owns the anchor, the source and
    /// the arithmetic -- and this writes a runtime field that managed clock does not read. **A new
    /// embedder calling this installs a time nothing will report**: `DateTime.UtcNow` goes on
    /// answering the epoch while the caller believes it set the clock, which is a wrong answer
    /// delivered silently.
    ///
    /// What still needs it is `clock_set_ticks`, the intrinsic an assembly compiled against an
    /// older corlib binds to. Such an assembly reads back through `datetime_now_ticks` in the same
    /// direction, so it is self-consistent on this storage -- one frozen path for images that
    /// predate the managed clock, and it never meets the managed one because an image carries its
    /// own corlib.
    pub fn set_now_ticks(&mut self, ticks: i64) {
        self.wall_anchor = Some((self.now_millis().unwrap_or(0), ticks));
        self.notify_wall_clock_sink(Some(ticks));
    }

    /// Tells the embedder's wall-clock sink what the wall clock was just set to: `Some(ticks)`, or
    /// `None` when it returned to never-set.
    ///
    /// The managed clock's `WallClockChanged` seam calls this on every set, whoever made it, and
    /// [`set_wall_clock`] calls it on behalf of a corlib that predates that seam. The state is
    /// recorded even when no sink is registered, so one registered later is handed it.
    pub fn notify_wall_clock_sink(&mut self, state: Option<i64>) {
        self.wall_told = state.map(|ticks| (self.now_millis().unwrap_or(0), ticks));
        self.wall_tellings = self.wall_tellings.wrapping_add(1);
        if let Some(sink) = self.wall_sink {
            sink(state);
        }
    }

    /// Registers the embedder's wall-clock sink: `sink` is told every set of the wall clock
    /// ([`Vm::notify_wall_clock_sink`]), and IMMEDIATELY the current wall reading when the clock
    /// is already set at registration -- so wiring order (seed the clock, then install the TLS
    /// backend, or the reverse) never loses a set. The device TLS bridge is the customer: the
    /// sink mirrors the managed clock into the statics the engine's time source reads, which is
    /// how "point the TLS time source at the clock `SystemClock` manages" is satisfied on a
    /// board whose only wall clock IS the managed one (adaptive TLS then full-validates
    /// certificate dates from the first session after a sync, and returns to its clockless
    /// bootstrap when the clock is told `None`).
    pub fn set_wall_clock_sink(&mut self, sink: fn(Option<i64>)) {
        self.wall_sink = Some(sink);
        if let Some((told_at, ticks)) = self.wall_told {
            let elapsed_millis = self.now_millis().unwrap_or(told_at).saturating_sub(told_at);
            let elapsed_ticks = i64::try_from(elapsed_millis)
                .unwrap_or(i64::MAX)
                .saturating_mul(10_000);
            sink(Some(ticks.saturating_add(elapsed_ticks)));
        }
    }

    /// Registers the embedder's pin-change event queue: the interpreter drains it between scheduler
    /// quanta and calls the managed dispatcher once per event.
    ///
    /// Installed by whatever owns the queue's STORAGE, which is also whatever exports
    /// `lamella_isr_notify` to the board's interrupt handler. Until this is called the drain is not
    /// attempted, so a build with no interrupts pays one `Option` test per scheduling decision.
    pub fn set_pin_event_source(&mut self, source: PinEventSource) {
        self.pin_events = Some(source);
    }

    /// Stores native key material and returns its HANDLE -- the only form the managed tier
    /// ever holds (a TLS exporter output, an AEAD key). The bytes stay runtime-side.
    pub fn insert_native_key(&mut self, bytes: Vec<u8>) -> i32 {
        self.native_keys.push(Some(bytes));
        (self.native_keys.len() - 1) as i32
    }

    /// The key bytes behind a native-key handle, or `None` for a bad/released handle.
    #[must_use]
    pub fn native_key(&self, handle: i32) -> Option<&[u8]> {
        usize::try_from(handle)
            .ok()
            .and_then(|index| self.native_keys.get(index))
            .and_then(|slot| slot.as_deref())
    }

    /// Releases a native key, ZEROIZING its bytes first so the material does not linger on
    /// the heap. A bad/released handle is a no-op.
    pub fn drop_native_key(&mut self, handle: i32) {
        let Ok(index) = usize::try_from(handle) else {
            return;
        };
        if let Some(slot) = self.native_keys.get_mut(index) {
            if let Some(bytes) = slot.as_mut() {
                bytes.iter_mut().for_each(|byte| *byte = 0);
            }
            *slot = None;
        }
    }

    /// The current wall time in 100-nanosecond ticks since the .NET epoch: the value last set by
    /// [`Vm::set_now_ticks`] PLUS the monotonic time elapsed since that set (so the clock advances).
    /// With no wall clock set it reads FORWARD FROM THE EPOCH by the monotonic clock, and with no
    /// clock seam at all it is 0. The `datetime_now_ticks` intrinsic returns this to the managed
    /// `DateTime.Now`/`UtcNow`.
    ///
    /// # Why the unset case still advances
    ///
    /// Two questions hide behind one reading. **What is the date** is unknown without an RTC, a seed
    /// or a sync, and the epoch is the honest answer to it. **How much time has passed** is a
    /// different question, and a board with a clock seam can answer it. Returning a CONSTANT for the
    /// unset case answered the second question wrongly in order to answer the first one honestly.
    ///
    /// That is not hypothetical: no serve firmware in this tree sets a wall clock, so `DateTime` was
    /// pinned to the epoch on EVERY board while `Environment.TickCount` -- the same seam, one
    /// accessor over -- advanced normally. A self-timing benchmark reported `0 ms` for real work and
    /// its checksum gate passed, which is a wrong answer delivered silently.
    ///
    /// [`Vm::clock_is_set`] still reports **false** here, and that split is load-bearing: this is not
    /// a claim about the date, and the adaptive TLS policy keys on `clock_is_set` to decide whether
    /// certificate dates can be validated. A clock reading year 1 must not start validating dates.
    #[must_use]
    pub fn now_ticks(&self) -> i64 {
        match self.wall_anchor {
            Some((anchor_ms, anchor_ticks)) => {
                let elapsed_ms = self.now_millis().unwrap_or(anchor_ms).saturating_sub(anchor_ms);
                anchor_ticks.saturating_add((elapsed_ms as i64).saturating_mul(10_000))
            }
            None => i64::try_from(self.now_millis().unwrap_or(0))
                .unwrap_or(i64::MAX)
                .saturating_mul(10_000),
        }
    }

    /// Whether the wall clock has been SET (via [`Vm::set_now_ticks`] -- an RTC, a seed, or a synced
    /// `SystemClock`) versus never set (reading the epoch). The managed `SystemClock.IsSet` reads
    /// it, and the adaptive TLS clock policy keys on the SAME clock through the embedder's TLS time
    /// source (the embedder points both at one clock: full cert-date validation once it is set,
    /// leap-of-faith only while unset). Kept in the RUNTIME (not a managed static) so it survives
    /// on AOT without a reference-assembly `.cctor`.
    #[must_use]
    pub fn clock_is_set(&self) -> bool {
        self.wall_anchor.is_some()
    }

    /// The monotonic millisecond count truncated to `int`, wrapping through `int.MinValue` after
    /// ~24.9 days as .NET's `Environment.TickCount` does. Zero without a clock seam. Backs the
    /// `Environment.get_TickCount` intrinsic.
    ///
    /// Counts from when the SYSTEM started, as .NET does. On a desktop the first reading is the
    /// machine's uptime and is usually large; on a board, which boots and runs one program, system
    /// uptime is that program's uptime and there is nothing else it could mean.
    ///
    ///
    /// Differences are unaffected, which is how nearly every caller uses this: two readings
    /// subtracted give the same elapsed milliseconds either way. Only an absolute reading differs,
    /// and on a board there is nothing else it could mean.
    #[must_use]
    pub fn tick_count(&self) -> i32 {
        self.now_millis().unwrap_or(0) as i32
    }

    /// Records `message` (a heap string) as `exception`'s message, for
    /// `Exception.Message`.
    pub fn set_exception_message(&mut self, exception: ObjectRef, message: ObjectRef) {
        self.exception_messages.insert(exception, message);
    }

    /// The recorded message string of `exception`, if any.
    #[must_use]
    pub fn exception_message(&self, exception: ObjectRef) -> Option<ObjectRef> {
        self.exception_messages.get(&exception).copied()
    }

    /// Records `inner` as the exception `exception` wraps, for `Exception.InnerException`.
    ///
    /// Side-state for the same reason the message is: a runtime-raised exception has no field
    /// storage at all, so a `TypeInitializationException` the VES mints cannot assign the
    /// `_innerException` field a managed constructor would have. Without this the wrapper's
    /// `InnerException` reads null, which is a divergence from .NET in a BCL class.
    #[cfg(feature = "exceptions")]
    pub(crate) fn set_exception_inner(&mut self, exception: ObjectRef, inner: ObjectRef) {
        self.exception_inners.insert(exception, inner);
    }

    /// The exception `exception` wraps, if the runtime recorded one.
    #[must_use]
    pub(crate) fn exception_inner(&self, exception: ObjectRef) -> Option<ObjectRef> {
        self.exception_inners.get(&exception).copied()
    }

    /// Records what the VES knows about a runtime-fault `exception`: its base-chain TAG vector
    /// (leaf-first up to `System.Object`, what a `catch` is matched against) and its leaf `Type`
    /// handle (what `GetType()` answers), which is `None` when that type is not among the loaded
    /// types -- a program running without the managed corlib names `System.DivideByZeroException`
    /// nowhere, so there is no handle to record and `GetType()` keeps its previous answer rather
    /// than inventing one. See [`FaultIdentity`] for the rest.
    #[cfg(feature = "exceptions")]
    fn set_exception_identity(&mut self, exception: ObjectRef, identity: FaultIdentity) {
        match self.exception_faults.iter_mut().find(|(key, _)| *key == exception) {
            Some(slot) => slot.1 = identity,
            None => self.exception_faults.push((exception, identity)),
        }
    }

    /// The recorded identity of a runtime-fault `exception`, if the VES minted it.
    #[cfg(feature = "exceptions")]
    fn exception_fault(&self, exception: ObjectRef) -> Option<&FaultIdentity> {
        self.exception_faults
            .iter()
            .find(|(key, _)| *key == exception)
            .map(|(_, fault)| fault)
    }

    /// The recorded base-chain tag vector of a runtime-fault `exception`, if one was set.
    #[cfg(feature = "exceptions")]
    #[must_use]
    fn exception_chain(&self, exception: ObjectRef) -> Option<&[u32]> {
        self.exception_faults
            .iter()
            .find(|(key, _)| *key == exception)
            .map(|(_, fault)| fault.chain.as_slice())
    }

    /// The type a RUNTIME-RAISED `exception` is an instance of: the one the VES recorded when it
    /// minted the object, which wears `EXTERNAL_TYPE_ID` in place of a type id of its own. `None`
    /// when nothing was recorded (its leaf type is not loaded).
    #[cfg(feature = "exceptions")]
    #[must_use]
    pub(crate) fn raised_exception_type(&self, module: &Module, exception: ObjectRef) -> Option<TypeId> {
        self.exception_type_handle(exception)
            .and_then(|handle| module.type_id_by_handle(handle))
    }

    /// Without `exceptions` the VES raises nothing catchable, so no object carries a recorded type.
    #[cfg(not(feature = "exceptions"))]
    #[must_use]
    pub(crate) fn raised_exception_type(&self, _module: &Module, _exception: ObjectRef) -> Option<TypeId> {
        None
    }

    /// The recorded `Type` handle of a runtime-fault `exception`, if its leaf type was loaded.
    #[cfg(feature = "exceptions")]
    #[must_use]
    pub(crate) fn exception_type_handle(&self, exception: ObjectRef) -> Option<u64> {
        self.exception_faults
            .iter()
            .find(|(key, _)| *key == exception)
            .and_then(|(_, fault)| fault.type_handle)
    }

    /// Records `exception` as the one currently propagating (set as a throw/fault begins its
    /// search), so an unhandled-exception pause can report it if the search exhausts the
    /// stack. The stepping loop discards it the moment a handler accepts (the unwind returns
    /// without a trap), so it cannot mis-attribute a later exception.
    #[cfg(feature = "exceptions")]
    fn note_unhandled(&mut self, exception: ObjectRef) {
        self.unhandled = Some(exception);
    }

    /// Takes the exception that escaped without a handler, if one did.
    #[cfg(feature = "exceptions")]
    fn take_unhandled(&mut self) -> Option<ObjectRef> {
        self.unhandled.take()
    }

    /// What this tier can say about the exception that escaped, for [`Trap::UnhandledException`].
    ///
    /// Fills each field only where this build genuinely knows it, so a caller can tell "the VES
    /// cannot say" from "the exception said nothing".
    ///
    pub(crate) fn unhandled_detail(&mut self, module: &Module) -> UnhandledException {
        #[cfg(not(feature = "exceptions"))]
        {
            let _ = module;
            return UnhandledException {
                tag: 0,
                type_name: None,
                message: None,
                raised_in: None,
                inner: None,
            };
        }
        #[cfg(feature = "exceptions")]
        {
        self.describe_exception(module, self.unhandled, 0)
        }
    }

    /// What the report can say about `escaped`: its type, its message, the method the VES raised
    /// it in, and the exception it wraps, described the same way. `None` describes nothing.
    #[cfg(feature = "exceptions")]
    fn describe_exception(
        &self,
        module: &Module,
        escaped: Option<ObjectRef>,
        depth: usize,
    ) -> UnhandledException {
        let type_id = escaped.and_then(|exception| match self.heap().type_of(exception) {
            Some(EXTERNAL_TYPE_ID) | None => self.raised_exception_type(module, exception),
            Some(type_id) => Some(type_id),
        });
        let fault = escaped.and_then(|exception| self.exception_fault(exception));
        let type_name = type_id
            .and_then(|id| module.type_full_name(id).map(String::from))
            .or_else(|| {
                fault
                    .map(|fault| fault.type_name)
                    .filter(|name| !name.is_empty())
                    .map(String::from)
            });
        let tag = type_id
            .and_then(|id| module.exception_tag_of(id))
            .or_else(|| fault.and_then(|fault| fault.chain.first().copied()))
            .unwrap_or(0);
        let message = escaped
            .and_then(|exception| {
                self.exception_message(exception)
                    .or_else(|| self.message_field_of(module, exception))
            })
            .and_then(|text| self.heap().as_string(text).map(|units| String::from_utf16_lossy(&units)));
        let raised_in = fault
            .and_then(|fault| fault.raised_in)
            .and_then(|method| module.method_type(method))
            .and_then(|type_id| module.type_full_name(type_id))
            .map(String::from);
        let inner = escaped
            .filter(|_| depth < MAX_REPORTED_INNER_EXCEPTIONS)
            .and_then(|exception| self.exception_inner(exception))
            .map(|inner| {
                alloc::boxed::Box::new(self.describe_exception(module, Some(inner), depth + 1))
            });
        UnhandledException { tag, type_name, message, raised_in, inner }
    }

    /// The message string a MANAGED constructor stored in an exception's own field, read from the
    /// slot the loader recorded for it.
    ///
    /// The runtime's own storage is asked first and this second, which is the order corlib's
    /// `Exception.Message` asks them in: a runtime-raised exception has no fields to read, and one
    /// managed code constructed has no entry in that storage, so the two are answering about
    /// different exceptions rather than disagreeing about one.
    #[cfg(feature = "exceptions")]
    fn message_field_of(&self, module: &Module, exception: ObjectRef) -> Option<ObjectRef> {
        let slot = module.exception_message_slot()?;
        match self.heap().instance_field(exception, slot) {
            Some(Value::Object(text)) => Some(text),
            _ => None,
        }
    }
}

/// Removes the first `units` UTF-16 units of `buffer` (all of them, if it holds fewer) and keeps its
/// capacity -- [`Vm::discard_output`] and [`Vm::discard_debug_output`].
fn discard_front(buffer: &mut Vec<u16>, units: usize) {
    let units = units.min(buffer.len());
    buffer.copy_within(units.., 0);
    buffer.truncate(buffer.len() - units);
}

/// A multicast delegate's bound method: a `(target, method)` pair.
type Invocation = (Value, u32);

/// A multicast `Invoke` in progress: the remaining invocations and the arguments each
/// is called with.
type Multicast = (Vec<Invocation>, Vec<Value>);

/// One activation frame: the evaluation stack, the local variables, the
/// arguments, the instruction pointer, and which method is running, for a single
/// method invocation.
struct Frame {
    method: MethodId,
    /// What every step of `method` reads from the module, resolved once when the frame is set up.
    context: MethodContext,
    ip: usize,
    /// The instruction whose execution can propagate an exception into this frame.
    /// Kept separately because a lazy initializer rewinds `ip` to replay its trigger.
    fault_ip: usize,
    stack: Vec<Value>,
    locals: Vec<Value>,
    args: Vec<Value>,
    /// Set on a constructor frame created by `newobj`: the new object to leave on
    /// the caller's stack when the frame returns (a ctor returns `void`, but the
    /// object reference is `newobj`'s result).
    new_object: Option<ObjectRef>,
    /// Set on a value-type constructor frame (`newobj` of a struct): the location of the
    /// temporary the ctor built in place, whose struct VALUE -- not a heap reference -- is
    /// left on the caller's stack when the frame returns.
    new_value: Option<Location>,
    /// The exception each catch handler of this frame caught, keyed by the handler's start, so a
    /// `rethrow` re-raises the one its own handler caught (III.4.24) and not whichever a nested
    /// handler caught last. Sized when the frame is set up ([`Frame::prepare_for_handlers`]), so
    /// recording a catch never allocates.
    caught_exceptions: Vec<(usize, ObjectRef)>,
    /// The in-progress `finally` chains (from a `leave` or an exception unwind), innermost last.
    ///
    /// One per `finally` or `fault` handler running, because a running handler can start a chain
    /// of its own: a `leave` out of a `try` inside it, which a `try`/`finally`, `using`, `lock` or
    /// `foreach` in a `finally` compiles to. That inner chain runs to its end first, and the
    /// handler's own `endfinally` then resumes the chain beneath it.
    pending: Vec<PendingFinally>,
    /// A `filter` expression being evaluated mid-unwind: the exception, the handler to
    /// enter if it accepts, and where to resume the search if it rejects.
    pending_filter: Option<PendingFilter>,
    /// A multicast-delegate invocation in progress: the remaining `(target, method)`
    /// invocations and the shared arguments, so each is called as the previous returns.
    multicast: Option<Multicast>,
    /// A `constrained.` prefix awaiting the next `callvirt`: the type to resolve the call
    /// against (the receiver stays a managed pointer to the value type).
    pending_constraint: Option<Token>,
    /// A `readonly.` prefix applies to the next non-prefix instruction.
    pending_readonly: bool,
    /// The frame's `localloc` (`stackalloc`) buffers: flat zeroed byte blocks a managed
    /// pointer (`Location::Stack`) indexes into. They live as long as the frame and are
    /// dropped with it on return, giving `stackalloc`'s lifetime. Empty for the common
    /// method that allocates none.
    buffers: Vec<Vec<u8>>,
    /// The variadic-argument region, set when a vararg call site invoked this frame: where the
    /// variable arguments begin in `args` and each one's type handle. `arglist` reads it to build a
    /// `TypedReference` per variable argument; `None` for the common non-vararg method.
    varargs: Option<VarargFrame>,
}

/// What a frame needs from its method on every step, resolved once per call rather than read from
/// the module before each instruction: the assembly whose token space the method's CIL resolves
/// against, and -- under `code-in-place`, when they outlive the module -- its instruction bytes.
#[derive(Clone, Copy)]
struct MethodContext {
    asm: u8,
    /// `None` for a body the module owns in RAM (a REPL delta), whose bytes `advance` reads from the
    /// module on each step.
    #[cfg(feature = "code-in-place")]
    code: Option<&'static [u8]>,
}

impl MethodContext {
    /// The context of a frame no module stands behind: [`run_method`]'s lone frame, which resolves
    /// no tokens.
    const NONE: MethodContext = MethodContext {
        asm: 0,
        #[cfg(feature = "code-in-place")]
        code: None,
    };

    fn of(module: &Module, id: MethodId) -> MethodContext {
        MethodContext {
            asm: module.method_asm(id),
            #[cfg(feature = "code-in-place")]
            code: module.method_code_static(id),
        }
    }
}

/// Recycled [`Frame`] shells and `Vec<Value>` buffers, so the steady-state cost of a
/// managed call is ZERO heap allocations: a returning frame's stack/locals/args buffers
/// (cleared, capacities kept) serve the next call instead of being dropped -- which on a
/// bump-allocator tier means leaked. Buffers grow to the program's high-water shape and
/// stay there; the pools cap so a pathological depth spike cannot hoard on a host.
///
/// Retired state is CLEARED (`len == 0` everywhere), so a pooled shell holds no object
/// references -- the collector, which roots only the live call stacks, never needs to see
/// the pool.
#[derive(Default)]
struct FramePool {
    /// Retired frame shells, ready to revive.
    shells: Vec<Frame>,
    /// Spare `Vec<Value>` buffers (call arguments in flight become frame `args`; the
    /// displaced buffer comes back here).
    values: Vec<Vec<Value>>,
}

impl FramePool {
    /// Pool caps: a shell per plausible call-stack slot, and a few argument buffers in
    /// flight. Past the cap a retiree is dropped -- correct on a host (the allocator
    /// frees), and on a bump tier the pool never exceeds the program's real peak anyway.
    const SHELLS_CAP: usize = 128;
    const VALUES_CAP: usize = 16;

    /// A cleared `Vec<Value>` buffer (a recycled one when available).
    fn take_values(&mut self) -> Vec<Value> {
        self.values.pop().unwrap_or_default()
    }

    /// Returns a `Vec<Value>` buffer to the pool (cleared here; capacity kept).
    fn give_values(&mut self, mut buffer: Vec<Value>) {
        if self.values.len() < Self::VALUES_CAP {
            buffer.clear();
            self.values.push(buffer);
        }
    }

    /// A frame for `method` taking ownership of `args` -- a revived shell when one is
    /// pooled (its displaced `args` buffer comes back to the pool), else a fresh frame.
    fn revive(&mut self, method: MethodId, context: MethodContext, args: Vec<Value>) -> Frame {
        match self.shells.pop() {
            Some(mut frame) => {
                frame.method = method;
                frame.context = context;
                frame.ip = 0;
                frame.fault_ip = 0;
                self.give_values(core::mem::replace(&mut frame.args, args));
                frame
            }
            None => Frame::new(method, context, args),
        }
    }

    /// Retires a finished frame: clear every buffer (keeping capacities) and every
    /// in-flight marker, then shelve it for the next call.
    fn retire(&mut self, mut frame: Frame) {
        if self.shells.len() >= Self::SHELLS_CAP {
            return;
        }
        frame.stack.clear();
        frame.locals.clear();
        frame.args.clear();
        frame.buffers.clear();
        frame.new_object = None;
        frame.new_value = None;
        frame.caught_exceptions.clear();
        frame.pending.clear();
        frame.pending_filter = None;
        frame.multicast = None;
        frame.pending_constraint = None;
        frame.pending_readonly = false;
        frame.varargs = None;
        self.shells.push(frame);
    }
}

impl core::fmt::Debug for FramePool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FramePool")
            .field("shells", &self.shells.len())
            .field("values", &self.values.len())
            .finish()
    }
}

/// Activation frames PARKED on the [`Vm`] for the duration of a nested interpreter run.
///
/// A [`Session`]'s root walk reaches that session's own frames and no others, so a collection
/// triggered inside a NESTED session -- a reflective `Invoke`, an intrinsic's lazy cctor, a
/// pin-change callback -- marks and remaps the nested frames while the caller's frames are left
/// pointing at the objects' old addresses. The caller's frames sit on the Rust stack for the
/// duration and no walk can reach them there, so the dispatch site hands them here first and takes
/// them back after: parked frames are roots like any other, marked and remapped by the same pass.
///
/// A STACK, because nesting nests: an intrinsic's cctor may call an intrinsic of its own. The
/// pin-event drain parks one entry per green thread, since every thread is suspended beneath it.
///
/// The frames are MOVED in and back out rather than borrowed, which is what lets an intrinsic keep
/// its `&mut Vm` -- the property the `Vm` doc calls out. The pool's shells are still not roots
/// (they are cleared on retirement); these are live frames temporarily housed elsewhere.
#[cfg(feature = "gc")]
#[derive(Default)]
struct ParkedFrames {
    /// One entry per suspended activation, innermost last.
    activations: Vec<Vec<Frame>>,
}

#[cfg(feature = "gc")]
impl core::fmt::Debug for ParkedFrames {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ParkedFrames")
            .field("activations", &self.activations.len())
            .finish()
    }
}

/// A vararg method frame's variadic region (set from the call site's [`crate::module::VarargSite`]).
#[cfg_attr(not(feature = "varargs"), allow(dead_code))]
struct VarargFrame {
    /// The `args` index at which the variable arguments begin (`this`? + fixed-parameter count).
    start: u16,
    /// Each variable argument's asm-folded type handle, in order.
    types: alloc::vec::Vec<u64>,
}

/// What executing one instruction decided.
#[cfg_attr(not(feature = "exceptions"), allow(dead_code))]
enum Flow {
    /// Continue with the next instruction (or wherever a branch set `ip`).
    Next,
    /// The method returned, with its result if any.
    Return(Option<Value>),
    /// `jmp`: replace the current frame with a tail call to this method, reusing the
    /// current frame's arguments.
    Jmp(MethodId),
    /// The method called another; its frame must be pushed.
    Call {
        /// The callee.
        method: MethodId,
        /// The arguments taken from the caller's stack, in declaration order.
        args: Vec<Value>,
    },
    /// A vararg `call`: like [`Flow::Call`] but the call site passed variable arguments beyond the
    /// callee's fixed parameters, so the pushed frame records its variadic region for `arglist`.
    CallVararg {
        /// The callee (the vararg method).
        method: MethodId,
        /// All arguments (`this`? + fixed + variable), taken from the caller's stack.
        args: Vec<Value>,
        /// The `args` index at which the variable arguments begin.
        vararg_start: u16,
        /// Each variable argument's asm-folded type handle, in order.
        var_types: Vec<u64>,
    },
    /// `newobj` allocated an object and must run its constructor: push a ctor frame
    /// with `this` (the new `object`) ahead of `args`, then leave `object` on the
    /// caller's stack when it returns.
    NewObj {
        /// The constructor to run.
        ctor: MethodId,
        /// The freshly allocated, zero-initialized object.
        object: ObjectRef,
        /// The constructor arguments (without `this`), from the caller's stack.
        args: Vec<Value>,
    },
    /// `newobj` of a String constructor over a raw `char*` (`String(char*)` /
    /// `String(char*, int, int)`): materialize the code units the pointer reaches and push
    /// the built string. Like [`Flow::LoadStack`], this defers to the frames-aware handler
    /// because the pointer may name ANOTHER frame's stackalloc buffer; no instance is
    /// pre-allocated and no ctor frame runs (a string is a dedicated heap object, not a
    /// field-holding instance a constructor could fill in).
    NewString {
        /// The `char*` argument -- a managed pointer (or a null/zero native int).
        pointer: Value,
        /// The `startIndex` argument in code units (0 for the one-argument form).
        start: i32,
        /// The `length` argument in code units; `None` for the one-argument form, which
        /// scans to the first U+0000 terminator instead.
        length: Option<i32>,
    },
    /// `newobj` of a value type: run its constructor against a managed pointer to a
    /// zero-initialized struct temporary, then leave that struct's VALUE (not a heap
    /// reference) on the caller's stack when it returns.
    NewValueObj {
        /// The constructor to run.
        ctor: MethodId,
        /// The managed pointer the ctor receives as `this` -- a temporary holding the
        /// zero struct, whose value is read back on return.
        location: Location,
        /// The constructor arguments (without `this`), from the caller's stack.
        args: Vec<Value>,
    },
    /// A type's first static access / method entry found a not-yet-run `.cctor`
    /// (II.10.5.3): the driver rewinds the triggering instruction and pushes the cctor's
    /// frame, so the cctor runs to completion and the instruction replays initialized.
    /// The type was already marked run (see [`Vm::take_pending_cctor`]).
    EnsureCctor(MethodId),
    /// A `throw` or `rethrow` is propagating an exception; the call stack must be
    /// searched for a handler.
    Throw(ObjectRef),
    /// A `leave`: exit a protected/handler region to this instruction index, after
    /// running any `finally` blocks being exited.
    Leave(usize),
    /// An `endfinally`: the current `finally` block is done; resume the chain.
    EndFinally,
    /// `endfilter`: a filter expression finished; the bool is whether it accepts (catch)
    /// or rejects (continue the handler search).
    EndFilter(bool),
    /// `ldfld` through a managed pointer (`&`): read a field of the value-type instance
    /// at `location`, which lives in the call stack `step` cannot reach.
    LoadField {
        /// The struct's location (a frame local/arg, or heap storage).
        location: Location,
        /// The field token (resolved to a slot by `advance`).
        field: Token,
    },
    /// `stfld` through a managed pointer (`&`): write a field of the value-type instance
    /// at `location`, materializing the struct if the slot is still empty.
    StoreField {
        /// The struct's location.
        location: Location,
        /// The field token.
        field: Token,
        /// The value to store.
        value: Value,
    },
    /// `initobj`: zero-initialize the value-type instance at `location` (a default
    /// struct).
    InitObj {
        /// The location to initialize.
        location: Location,
        /// The value type's token (giving its zero fields).
        kind: Token,
    },
    /// `ldobj`: load the value-type instance at `location` onto the evaluation stack.
    LoadObj {
        /// The location to read.
        location: Location,
        /// The triggering opcode, so a narrow `ldind.i1`/`u1`/`i2`/`u2` re-narrows the loaded value
        /// to its width (truncate + sign/zero-extend). `Ldobj` and the wide `ldind`s are no-ops.
        opcode: Opcode,
        /// The type `ldobj` names; `Token(0)` for an `ldind`.
        token: Token,
    },
    /// `stobj`: store a value-type instance through a managed pointer to `location`.
    StoreObj {
        /// The location to write.
        location: Location,
        /// The value to store.
        value: Value,
        /// The instruction storing it: `stobj`, or the `stind.<type>` whose width the store has.
        opcode: Opcode,
        /// The type `stobj` names; `Token(0)` for a `stind`.
        token: Token,
    },
    /// `cpobj`: copy the value-type instance at `src` to `dest` (both managed pointers).
    CopyObj {
        /// The destination location to write.
        dest: Location,
        /// The source location to read.
        src: Location,
    },
    /// A cross-frame `ldind` through a localloc pointer: read `width` bytes (per `opcode`,
    /// little-endian) from localloc `buffer` of the frame at `owner`, which is NOT the current
    /// frame (a `stackalloc` buffer passed to a callee and dereferenced there). Byte-width access
    /// needs the owning frame's stack, so -- like [`LoadObj`] for the heap -- it defers to the
    /// frames-aware handler.
    LoadStack {
        /// The owning frame's index in the call stack.
        owner: usize,
        /// Which of that frame's localloc buffers.
        buffer: usize,
        /// The byte offset within it.
        offset: u32,
        /// The triggering `ldind` (its width + sign/zero extension).
        opcode: Opcode,
    },
    /// A cross-frame `stind` through a localloc pointer: write `value` at `width` (per `opcode`)
    /// into localloc `buffer` of the frame at `owner`, which is NOT the current frame.
    StoreStack {
        /// The owning frame's index in the call stack.
        owner: usize,
        /// Which of that frame's localloc buffers.
        buffer: usize,
        /// The byte offset within it.
        offset: u32,
        /// The triggering `stind` (its width + narrowing).
        opcode: Opcode,
        /// The value to store.
        value: Value,
    },
    /// A multicast delegate's `Invoke`: call each `(target, method)` in turn (each with
    /// `params`); the delegate's result is the last one's.
    InvokeMulticast {
        /// The bound methods to call, in order.
        invocations: Vec<(Value, u32)>,
        /// The arguments shared by every call (the delegate's parameters).
        params: Vec<Value>,
    },
}

/// What to do once a chain of `finally` blocks (run by a `leave` or an exception)
/// finishes.
enum AfterFinally {
    /// A `leave`: branch to this instruction index.
    Goto(usize),
    /// The exception was caught in this frame: enter the catch handler with it.
    Catch {
        /// The catch handler's first instruction.
        handler: usize,
        /// The exception to hand the handler.
        exception: ObjectRef,
    },
    /// The exception was not caught in this frame: pop the frame and keep unwinding.
    Unwind(ObjectRef),
}

/// A frame's in-progress `finally` chain: the handler it is running, the handlers still to run
/// (innermost first, via `pop`), and what to do once they are all done.
struct PendingFinally {
    /// The `finally` or `fault` handler this chain is running. An exception caught outside it
    /// leaves it for good, which is how [`abandon_finallys_for`] knows the chain is over.
    running: InstructionRange,
    finallys: Vec<InstructionRange>,
    then: AfterFinally,
}

/// A `filter` expression being evaluated during the handler search for an exception.
struct PendingFilter {
    /// The exception being filtered.
    exception: ObjectRef,
    /// The handler to enter if the filter accepts (leaves a non-zero result).
    handler: usize,
    /// The filter's try region, for the finallys to run before entering its handler.
    filter_try: InstructionRange,
    /// The clause index to resume the search from if the filter rejects.
    resume: usize,
    /// The original fault site, preserved across the filter's evaluation.
    fault_ip: usize,
}

/// Runs a single method that makes no calls, returning the value its `ret` leaves
/// on the stack (`None` for a void return).
///
/// For methods that call others, use [`run`] with a [`Module`]. A `call` here is
/// [`Trap::Unsupported`], since there is no module to resolve it against.
///
/// # Errors
/// Returns a [`Trap`] for malformed or unsupported CIL, a stack imbalance, an
/// out-of-range local, argument, or branch, or integer division by zero.
pub fn run_method(body: &MethodBodyImage, args: Vec<Value>) -> Result<Option<Value>, Trap> {
    let mut vm = Vm::new();
    let mut frame = Frame::new(0, MethodContext::NONE, args);
    loop {
        let instruction = body.code.get(frame.ip).ok_or(Trap::FellThroughEnd)?;
        frame.ip += 1;
        match step(&mut frame, 0, &[], body.code.len(), None, &mut vm, instruction)? {
            Flow::Next => {}
            Flow::Return(result) => return Ok(result),
            Flow::Call { .. } | Flow::CallVararg { .. } => {
                return Err(Trap::Unsupported(Opcode::Call))
            }
            Flow::NewObj { .. } | Flow::NewValueObj { .. } | Flow::NewString { .. } => {
                return Err(Trap::Unsupported(Opcode::Newobj));
            }
            Flow::EnsureCctor(_) => return Err(Trap::Unsupported(Opcode::Ldsfld)),
            Flow::Throw(_) => return Err(Trap::Unsupported(Opcode::Throw)),
            Flow::Leave(_) => return Err(Trap::Unsupported(Opcode::Leave)),
            Flow::EndFinally => return Err(Trap::Unsupported(Opcode::Endfinally)),
            Flow::EndFilter(_) => return Err(Trap::Unsupported(Opcode::Endfilter)),
            Flow::Jmp(_) => return Err(Trap::Unsupported(Opcode::Jmp)),
            Flow::LoadField { .. } => return Err(Trap::Unsupported(Opcode::Ldfld)),
            Flow::StoreField { .. } => return Err(Trap::Unsupported(Opcode::Stfld)),
            Flow::InitObj { .. } => return Err(Trap::Unsupported(Opcode::Initobj)),
            Flow::LoadObj { .. } => return Err(Trap::Unsupported(Opcode::Ldobj)),
            Flow::StoreObj { .. } => return Err(Trap::Unsupported(Opcode::Stobj)),
            Flow::CopyObj { .. } => return Err(Trap::Unsupported(Opcode::Cpobj)),
            Flow::LoadStack { opcode, .. } | Flow::StoreStack { opcode, .. } => {
                return Err(Trap::Unsupported(opcode));
            }
            Flow::InvokeMulticast { .. } => return Err(Trap::Unsupported(Opcode::Callvirt)),
        }
    }
}

/// The cooperative scheduler's deterministic time slice: a thread is preempted after this many CIL
/// instructions in a multi-threaded run (checked at every interp instruction boundary, which is a GC
/// safe point). Op-count, not wall-clock, so interleavings reproduce host-and-device. K in the
/// 256-1024 range; 256 keeps switch latency well under a quantum.
const TIME_SLICE_QUANTUM: u32 = 256;

/// How long the scheduler idles between looks for pin events and carrier traffic when nothing can be
/// waited on but a thread sleeps forever (`Thread.Sleep(Timeout.Infinite)`). 16 ms is the slice every
/// board's own sleep already takes, so a program parked for good answers at the cadence a program in
/// a long timed sleep does.
const FOREVER_IDLE_SLICE_MS: u64 = 16;

/// A scheduler request an intrinsic raises; the scheduler takes it once the running thread pauses.
#[derive(Debug)]
enum ThreadOp {
    /// Spawn a green thread (id `id`) running `method` with `target` as its receiver; `background`
    /// marks a daemon thread (a `Timer`) the run loop abandons once the foreground threads finish.
    Spawn { id: u32, method: MethodId, target: Value, background: bool },
    /// Cooperatively yield the running thread.
    Yield,
    /// Block the running thread until thread `target` completes, or -- with a `deadline` (monotonic
    /// ms, a timed `Thread.Join`) -- until that passes, whichever is first. `None` waits however
    /// long it takes, which is the untimed `Join()`.
    Join { target: u32, deadline: Option<u64> },
    /// Block the running thread on a `Monitor` lock -- a failed acquire (queued in the lock's
    /// waiters) or a `Monitor.Wait` (parked in the condition wait-set). The scheduler parks it until
    /// a later wake hands it the lock. `wake`, if set, is a contender that a `Monitor.Wait`'s release
    /// just handed the lock -- the scheduler makes it runnable in the same step.
    BlockOnLock {
        wake: Option<u32>,
        /// A TIMED `Monitor.Wait`'s bound (monotonic ms): the reactor also wakes the thread when it
        /// passes. `None` = wait until pulsed, however long that is.
        deadline: Option<u64>,
    },
    /// Wake thread `id`, blocked on a `Monitor` lock that a release just handed to it.
    WakeThread { id: u32 },
    /// Block the running thread until `now` + this many milliseconds (`Thread.Sleep`).
    SleepFor(u64),
    /// Park the running thread for good (`Thread.Sleep(Timeout.Infinite)`).
    SleepForever,
    /// Block the running thread on socket I/O: register `socket` for `interest` with the reactor and
    /// park until its `poll` reports the socket ready (a socket op that would block). A receive
    /// timeout, when installed for the socket, bounds the park with a wake `deadline` (monotonic ms).
    BlockOnIo {
        socket: u32,
        interest: crate::net::Interest,
        deadline: Option<u64>,
    },
}

/// The state of one entered `Monitor` lock. Lives in [`Vm::locks`], keyed by the lock object's heap
/// index. Unlike the old atomic-section approach, a lock can outlive a scheduler switch (the holder
/// may be preempted inside its critical section, and `Monitor.Wait` parks the owner while OTHER
/// threads run), so this state persists across switches and GCs -- hence the table's keys are
/// remapped at every collection.
#[derive(Debug)]
struct LockState {
    /// The owning thread, or `None` when the lock is momentarily unowned -- the entry then exists
    /// only because `waiting` is non-empty (a `Monitor.Wait`-er released it with no one contending).
    owner: Option<u32>,
    /// The owner's recursion depth (recursive `Enter`s); 0 exactly when `owner` is `None`.
    count: u32,
    /// Threads blocked trying to ACQUIRE the lock, FIFO; each is handed the lock (restoring its
    /// recorded depth) when the owner releases.
    waiters: Vec<LockWaiter>,
    /// Threads blocked in `Monitor.Wait` until a `Pulse`/`PulseAll` moves them into `waiters`.
    waiting: Vec<LockWaiter>,
}

/// A thread queued on a lock, with the recursion depth to restore when it is granted the lock -- 1
/// for a plain contended `Enter`, or the owner's saved depth for a thread released by `Monitor.Wait`
/// (so a recursive `Wait` reacquires at the depth it left).
#[derive(Debug)]
struct LockWaiter {
    thread: u32,
    depth: u32,
    /// Whether this waiter may ABANDON the acquire-queue when its deadline fires.
    ///
    /// A timed `Monitor.TryEnter` may: it promised an answer within the timeout and its answer is
    /// "no, you do not have the lock". A waiter a `Pulse` moved here from the condition wait-set may
    /// NOT -- .NET's `Wait` reacquires the lock before returning EVEN WHEN IT TIMED OUT, so
    /// abandoning would return it to managed code without the lock its next line assumes it holds.
    ///
    /// The two sit in the SAME queue with opposite obligations, which is exactly why this is
    /// recorded at enqueue rather than inferred from where the thread is found.
    abandonable: bool,
}

impl LockState {
    /// A fresh lock held once by `thread`, with empty queues.
    fn owned_by(thread: u32) -> LockState {
        LockState { owner: Some(thread), count: 1, waiters: Vec::new(), waiting: Vec::new() }
    }

    /// Hands the lock to the first acquire-waiter (FIFO), restoring its recorded depth, and returns
    /// its id; if none are waiting, leaves the lock unowned (`owner = None`, depth 0) and returns
    /// `None`.
    fn grant_to_next_waiter(&mut self) -> Option<u32> {
        if self.waiters.is_empty() {
            self.owner = None;
            self.count = 0;
            return None;
        }
        let next = self.waiters.remove(0);
        self.owner = Some(next.thread);
        self.count = next.depth;
        Some(next.thread)
    }
}

/// One green thread in the cooperative scheduler: a reified [`Session`] plus its scheduling state
/// and (once finished) its result.
struct ThreadSlot {
    id: u32,
    session: Session,
    state: ThreadState,
    result: Option<Option<Value>>,
    /// A background (daemon) thread -- a `Timer`'s. The scheduler abandons it once every foreground
    /// thread has finished, rather than waiting for it (.NET's background-thread semantics).
    background: bool,
}

#[derive(PartialEq, Eq)]
enum ThreadState {
    /// Runnable.
    Ready,
    /// Blocked until the thread with this id finishes, or -- when a DEADLINE is set (a timed
    /// `Thread.Join`) -- until that monotonic-millisecond deadline passes, whichever comes first.
    ///
    /// A deadline wake here is unambiguous, unlike [`BlockedUntil`](Self::BlockedUntil)'s: the only
    /// thing that clears a join is the target FINISHING, and no thread runs during the reactor's
    /// block point, so a joiner the reactor wakes really did run out of time. It is recorded as a
    /// timed-park verdict for the managed `Join(int)` to read.
    Joining(u32, Option<u64>),
    /// Blocked on a `Monitor` lock; set back to `Ready` by a [`ThreadOp::WakeThread`] when the
    /// lock's owner releases and hands it over.
    Blocked,
    /// Blocked against a lock with a DEADLINE: a timed `Monitor.Wait` parked in the condition
    /// wait-set, or a timed `Monitor.TryEnter` queued for the lock. Both park exactly like
    /// [`Blocked`](Self::Blocked) and are ALSO woken by the reactor once this monotonic-millisecond
    /// deadline passes; both answer the same question, "did I get it in time".
    ///
    /// A deadline wake is NOT simply `Ready` -- a timed-out `Wait` must reacquire the lock first,
    /// and a thread the hand-off already reached is not timed out at all. See
    /// [`Vm::resolve_lock_deadline`], which decides between them.
    BlockedUntil(u64),
    /// Sleeping until this monotonic-millisecond deadline (`Thread.Sleep`); woken by the scheduler's
    /// `idle_wait` once the deadline passes.
    Sleeping(u64),
    /// Asleep for good (`Thread.Sleep(Timeout.Infinite)`). It has no deadline, so the reactor never
    /// sees it and nothing wakes it; while one exists the scheduler never reads "nothing left to wait
    /// for" as the end of the program, and idles instead, still delivering pin events.
    SleepingForever,
    /// Blocked on socket I/O -- parked until this socket handle becomes ready, or (when a receive
    /// timeout bounds the park) until the deadline passes. Set by a socket op that would block;
    /// woken in `idle_wait` when the reactor's poll reports the socket ready or the deadline hits
    /// (the managed op then re-polls and decides whether expiry is an error).
    IoWait(u32, Option<u64>),
    /// Finished (its `result` is set).
    Done,
}

/// Whether a thread is parked on a `Monitor` lock -- untimed or with a deadline. Both are woken the
/// same way by a lock HAND-OFF; only the reactor tells them apart.
fn is_lock_blocked(state: &ThreadState) -> bool {
    matches!(state, ThreadState::Blocked | ThreadState::BlockedUntil(_))
}

/// The park a bounded idle wait adds for its bound ([`idle_wait`]): an id no thread has.
const BOUNDED_WAIT: u32 = u32::MAX;

/// The next runnable thread at or after `start` (round-robin, wrapping); `None` once every thread is
/// finished or blocked.
fn next_ready_thread(threads: &[ThreadSlot], start: usize) -> Option<usize> {
    let count = threads.len();
    (0..count)
        .map(|offset| (start + offset) % count)
        .find(|&index| threads[index].state == ThreadState::Ready)
}

/// The interpreter side of the reactor seam: `Vm` supplies the clock + network the block point
/// blocks on. Each method delegates to `Vm`'s inherent one (the UFCS `Vm::method(self, ..)` calls
/// pick those, not this trait's, so there is no recursion).
impl crate::reactor::ReactorEnv for Vm {
    fn now_millis(&self) -> Option<u64> {
        Vm::now_millis(self)
    }
    fn sleep_millis(&mut self, millis: u64) {
        Vm::sleep_millis(self, millis);
    }
    fn net_poll(&mut self, timeout_ms: Option<u64>) -> alloc::vec::Vec<u32> {
        Vm::net_poll(self, timeout_ms)
    }
    fn net_deregister(&mut self, socket: u32) {
        Vm::net_deregister(self, socket);
    }
}

/// The scheduler's reactor idle-hook: with no thread ready, perform the ONE blocking wait (the
/// nearest timer deadline and/or the socket poll-set) and wake the threads the reactor reports
/// ready. The algorithm lives Session-free in [`crate::reactor`] so the AOT native scheduler drives
/// the SAME impl; here it is fed the interpreter's parked threads and applies the wake back to their
/// slots. Returns `false` when nothing is waitable -- the remaining threads are lock/join-deadlocked
/// and the scheduler stops. The single OS block point / tickless-idle foundation.
///
/// `until` bounds the wait at that machine-clock time, for a debug run that must come back to its
/// debugger ([`Threads::debug_run`]): the wait then ends there whether or not a thread woke. A bound
/// never makes something waitable, so a program with nothing to wait for still answers `false`.
fn idle_wait(threads: &mut [ThreadSlot], vm: &mut Vm, until: Option<u64>) -> bool {
    use crate::reactor::WaitReason;
    let mut parks: alloc::vec::Vec<(u32, WaitReason)> = alloc::vec::Vec::new();
    for slot in threads.iter() {
        match slot.state {
            ThreadState::Sleeping(deadline) => parks.push((slot.id, WaitReason::Sleep(deadline))),
            ThreadState::BlockedUntil(deadline) => parks.push((slot.id, WaitReason::Sleep(deadline))),
            ThreadState::Joining(_, Some(deadline)) => {
                parks.push((slot.id, WaitReason::Sleep(deadline)));
            }
            ThreadState::IoWait(handle, deadline) => {
                parks.push((slot.id, WaitReason::Io(handle)));
                if let Some(deadline) = deadline {
                    parks.push((slot.id, WaitReason::Sleep(deadline)));
                }
            }
            _ => {}
        }
    }
    if let Some(until) = until.filter(|_| !parks.is_empty()) {
        parks.push((BOUNDED_WAIT, crate::reactor::WaitReason::Sleep(until)));
    }
    match crate::reactor::block_point(&parks, vm) {
        None => false,
        Some(woken) => {
            for slot in threads.iter_mut() {
                if woken.contains(&slot.id) {
                    if let ThreadState::IoWait(handle, Some(_)) = slot.state {
                        vm.net_deregister(handle);
                    }
                    if let ThreadState::BlockedUntil(_) = slot.state {
                        if vm.resolve_lock_deadline(slot.id) {
                            slot.state = ThreadState::Ready;
                        } else {
                            slot.state = ThreadState::Blocked;
                        }
                        continue;
                    }
                    if let ThreadState::Joining(_, Some(_)) = slot.state {
                        vm.record_park_timeout(slot.id);
                    }
                    slot.state = ThreadState::Ready;
                }
            }
            true
        }
    }
}

/// Why [`Session::run_until_yield`] stopped: the thread finished (with its result), or it hit a
/// cooperative yield point and the scheduler should take the pending thread-op.
enum ThreadStatus {
    Done(Option<Value>),
    Yielded,
    /// The service callback asked the scheduler to stop -- the embedder is taking the machine back
    /// (a host `HELLO` reclaiming a board from the app it deployed). In a debug run, also the run
    /// stopping the thread part-way through its turn, for the reason it leaves in
    /// [`Watching::suspension`]; the turn goes on from there at the next call.
    Interrupted,
}

/// Why a debug run stopped a thread part-way through its turn ([`Watching::suspension`]).
#[derive(Clone, Copy)]
enum Suspension {
    /// The thread is about to run an instruction with an enabled breakpoint, which it has not run.
    Breakpoint,
    /// The thread the run watches has run its one instruction.
    Stepped,
    /// The run has spent its instructions or reached its time.
    Spent,
}

/// Why the scheduler ended a debug run's slice short of the program's end ([`Watching::end`]): every
/// [`Slice`] except [`Slice::Finished`].
#[derive(Clone, Copy)]
enum Ended {
    /// See [`Slice::Running`].
    Running,
    /// See [`Slice::Breakpoint`].
    Breakpoint(u32),
    /// See [`Slice::Stepped`].
    Stepped(u32),
    /// See [`Slice::Waiting`].
    Waiting,
}

/// What a debug run watches for, as the thread taking a turn sees it ([`Session::run_until_yield`]).
#[derive(Default)]
struct Watching {
    /// The thread a step watches: it stops once it has run one instruction.
    step: Option<u32>,
    /// Whether the thread taking this turn is the one [`Watching::step`] names. Its next instruction
    /// runs although a breakpoint is set on it, as a step moves off whatever instruction it is on.
    watched: bool,
    /// Whether the thread taking this turn runs its next instruction although a breakpoint is set on
    /// it: the thread a debugger reported stopped there, going on.
    stepping_off: bool,
    /// Whether the watched thread has run its instruction.
    stepped: bool,
    /// The instructions the run may still take, across every thread.
    budget: u32,
    /// The machine-clock time (monotonic milliseconds) the run ends at, where the machine has a clock.
    until: Option<u64>,
    /// Why the run stopped the thread taking its turn part-way through it, once it has.
    suspension: Option<Suspension>,
    /// Why the scheduler ended the run's slice, once it has ([`Watching::end`]).
    ended: Option<Ended>,
}

impl Watching {
    /// Whether the run has reached its time.
    fn past(&self, vm: &Vm) -> bool {
        match (self.until, vm.now_millis()) {
            (Some(until), Some(now)) => now >= until,
            _ => false,
        }
    }

    /// Whether the run's time can be told: it names one, and the machine has a clock.
    fn timed(&self, vm: &Vm) -> bool {
        self.until.is_some() && vm.now_millis().is_some()
    }

    /// Ends the slice for `why`. The scheduler returns that as its run's interruption, and
    /// [`Threads::debug_run`] answers the reason left here.
    fn end(&mut self, why: Ended) -> Ran {
        self.ended = Some(why);
        Ran::Interrupted
    }

    /// Stops the thread taking its turn part-way through it, for `why`: what the thread returns as
    /// its turn's interruption, the reason left here for the scheduler.
    fn suspend(&mut self, why: Suspension) -> ThreadStatus {
        self.suspension = Some(why);
        ThreadStatus::Interrupted
    }
}

/// Installs the wall clock through the MANAGED setter -- `Lamella.Runtime.Clock::SetTicks(long)` --
/// and notifies the embedder's wall-clock sink. `false` when the loaded module declares no such
/// setter, which is every module that carries no corlib.
///
/// # Why an embedder calls this and not [`Vm::set_now_ticks`]
///
/// The wall clock's whole state is managed: the anchor, the source and the
/// arithmetic live in `Lamella.Runtime.Clock`, so writing a runtime field would write something the
/// managed clock does not read -- `DateTime.UtcNow` would go on answering the epoch while the
/// embedder believed it had set the time. That is the silent divergence this shape exists to
/// remove, and it is why the door INVOKES the setter rather than poking its statics: the anchoring
/// rule stays in one place instead of gaining a second copy here.
///
/// # Errors
/// None; a trap inside the setter is reported as `false` rather than propagated, because an
/// embedder seeding a clock has nothing to unwind and the truthful outcome is that the clock is
/// not set -- which `Clock.IsSet` will then say on its own.
pub fn set_wall_clock(module: &Module, vm: &mut Vm, ticks: i64) -> bool {
    let Some(setter) = module.wall_clock_setter() else {
        return false;
    };
    let tellings = vm.wall_tellings;
    if run(module, vm, setter, alloc::vec![Value::Int64(ticks)]).is_err() {
        return false;
    }
    if vm.wall_tellings == tellings {
        vm.notify_wall_clock_sink(Some(ticks));
    }
    true
}

/// Drains the embedder's pin-event queue into the managed dispatcher --
/// `Lamella.Hardware.PinEvents::Dispatch(int, bool)` -- at most
/// [`lamella_pin_events::MAX_EVENTS_PER_DRAIN`] events, then reports a drop if there was one.
///
/// Does nothing when no queue is installed, when the loaded module declares no dispatcher (a
/// program referencing no GPIO assembly, and every floor-tier build, which has none), or when the
/// queue is empty -- the ordinary case, and it costs one fn-pointer call.
///
/// # Why the interpreter drains this rather than the embedder
///
/// An embedder's loop only runs between top-level calls, and a deployed program that never returns
/// is exactly the program a pin-change handler is for. The drain has to happen while the program is
/// RUNNING, which means it happens where the scheduler already decides what runs next.
///
/// # The program's threads stay roots while a callback runs
///
/// A callback runs on a nested [`run`], and that run's root walk reaches its own frames, not those of
/// the `threads` the scheduler is holding meanwhile. So each thread's frames are parked on the [`Vm`]
/// for the length of the drain ([`ParkedFrames`]), and a collection that a callback triggers marks
/// and relocates them with everything else, rather than reclaiming what only they reach.
///
/// # Errors
/// Propagates a [`Trap`] from the dispatcher or from a handler it invokes. An exception a callback
/// does not handle ends the program, which is what it does on .NET; swallowing it here would leave
/// a program running with a handler that had silently stopped working.
fn drain_pin_events(module: &Module, vm: &mut Vm, threads: &mut [ThreadSlot]) -> Result<(), Trap> {
    let Some(source) = vm.pin_events else { return Ok(()) };
    let Some(dispatch) = module.pin_event_dispatch() else { return Ok(()) };
    if vm.pin_events_draining {
        return Ok(());
    }
    vm.pin_events_draining = true;
    #[cfg(feature = "gc")]
    for slot in threads.iter_mut() {
        vm.park_frames(core::mem::take(&mut slot.session.frames));
    }
    #[cfg(not(feature = "gc"))]
    let _ = &threads;
    let outcome = dispatch_queued(module, vm, source, dispatch);
    #[cfg(feature = "gc")]
    for slot in threads.iter_mut().rev() {
        slot.session.frames = vm.unpark_frames();
    }
    vm.pin_events_draining = false;
    outcome
}

/// The body of [`drain_pin_events`], split out so the re-entrancy flag has exactly one clear.
fn dispatch_queued(
    module: &Module,
    vm: &mut Vm,
    source: PinEventSource,
    dispatch: MethodId,
) -> Result<(), Trap> {
    for _ in 0..lamella_pin_events::MAX_EVENTS_PER_DRAIN {
        let Some(event) = (source.next)() else { break };
        run(
            module,
            vm,
            dispatch,
            alloc::vec![Value::Int32(event.token as i32), Value::Int32(i32::from(event.level))],
        )?;
    }
    if (source.drain_overflowed)() {
        if let Some(report) = module.pin_event_lost() {
            run(module, vm, report, alloc::vec![])?;
        }
    }
    Ok(())
}

/// The managed wall clock's SOURCE as its underlying int -- 0 is `ClockSource.Unset`, i.e. never
/// set. `None` when the loaded module declares no such reader.
///
/// The read half of [`set_wall_clock`], and it answers the SOURCE rather than a boolean on purpose:
/// whether the clock is set and WHERE it came from are different questions, and a caller deciding
/// how far to trust a date is asking the second. A board that read a dead coin cell's RTC and one
/// that completed a network sync both report "set" and are not equally trustworthy.
///
/// # Errors
/// None; a trap inside the reader is reported as `None`, which a caller reads the same way as a
/// module that declares no clock at all -- in both cases there is no source to report.
#[must_use]
pub fn wall_clock_source(module: &Module, vm: &mut Vm) -> Option<i32> {
    let reader = module.wall_clock_source()?;
    match run(module, vm, reader, alloc::vec![]) {
        Ok(Some(Value::Int32(source))) => Some(source),
        _ => None,
    }
}

/// Runs `module`'s `entry` (with `args`) to completion, returning its result: [`run_serviced`]
/// with no service callback.
///
/// Execution is driven by a green-thread scheduler: the entry is thread 0 (the main thread), and a
/// `Thread.Start` spawns another thread as a fresh [`Session`] (a thread is a reified session). The
/// scheduler steps the running thread until it finishes, reaches a yield point (`Thread.Yield` /
/// `Sleep` / `Join`, a contended lock, a blocking socket, signalled through the [`Vm`]'s thread-op
/// channel) or, while another thread is live, spends its time slice of CIL instructions; then it
/// picks the next runnable thread. With a single thread -- the common case, and every program that
/// spawns none -- the loop reduces to running thread 0 to completion.
///
/// `Thread.Start`, `Sleep` and `Join` are the Base Class Library's (ECMA-335 6th ed IV.5.3; the
/// members are specified in its CLILibraryTypes.xml): a started thread becomes ready to run, and
/// `Sleep` and `Join` block the calling thread. `Thread.Yield` is .NET's, outside that library.
///
/// # Errors
/// Propagates a [`Trap`] from any thread's execution, and returns [`Trap::StaticCtorsNotRun`] for
/// an image whose static constructors run before its entry point when [`boot_baked`] has not run
/// them. When a type's initializer may run is ECMA-335 6th ed I.8.9.5.
pub fn run(
    module: &Module,
    vm: &mut Vm,
    entry: MethodId,
    args: Vec<Value>,
) -> Result<Option<Value>, Trap> {
    run_serviced(module, vm, entry, args, &mut |_vm| {})
}

/// Prepares a module's type initialization before its entry point runs, and hands `entry` back
/// so the entry point a caller runs is the one that came out of the boot.
///
/// A module that fires each static constructor at its type's first access
/// ([`Module::initializes_types_on_first_access`]) needs nothing here, and nothing runs: every
/// loaded module, and every image baked since the trigger tables were added. A constructor then
/// runs where ECMA-335 II.10.5.3 puts it, so its output follows `Main`'s and one that throws
/// raises `TypeInitializationException` at the access, where the program can catch it. A board's
/// class is no exception: its initializer binds the board's buses where the program first touches
/// the class.
///
/// An image baked before those tables has no trigger to fire, so its static constructors run
/// here, in declaration order, before the entry point. Without that no static would ever
/// initialize -- so [`run`] refuses such a module unless this has run.
///
/// Configure the [`Vm`] FIRST -- board seams, console, heap. A static constructor observes
/// the VM it runs on, so a seam installed afterwards is not there when it runs.
///
/// # Errors
/// Propagates a [`Trap`] from a static constructor run here.
pub fn boot_baked(module: &Module, vm: &mut Vm, entry: MethodId) -> Result<MethodId, Trap> {
    if !module.initializes_types_on_first_access() {
        vm.mark_all_cctors_run(module);
        for &cctor in module.static_ctors() {
            run(module, vm, cctor, Vec::new())?;
        }
    }
    Ok(entry)
}

/// The arguments the entry point `entry` starts with, as .NET passes them: a `Main(string[] args)`
/// gets a `string[]` of the program's arguments ([`Vm::set_program_args`]), empty when there are
/// none, and a `Main()` gets nothing. ECMA-335 II.15.4.1.2 allows an entry point exactly those two
/// shapes.
///
/// A host that starts a program passes this to [`run`], [`run_interruptible`] or [`Session::new`].
/// An empty list instead leaves a `Main(string[] args)` without its argument, so its first read of
/// `args` traps.
#[must_use]
pub fn entry_args(module: &Module, vm: &mut Vm, entry: MethodId) -> Vec<Value> {
    if module.method(entry).map_or(0, crate::module::Method::arg_count) == 0 {
        return Vec::new();
    }
    let program_args = vm.program_args.clone();
    let elements = program_args
        .iter()
        .map(|arg| Value::Object(vm.heap_mut().alloc_text(arg)))
        .collect();
    alloc::vec![Value::Object(vm.heap_mut().alloc_array(elements))]
}

/// [`run`] plus a `service` callback fired at EVERY scheduler quantum boundary (every
/// [`TIME_SLICE_QUANTUM`] instructions), single- OR multi-threaded. A device serve passes a
/// USB-poll closure so a running program's tight loop cannot starve the POLLED Lamella Link: a
/// single-threaded program that never yields otherwise runs to completion without the scheduler
/// ever regaining control, so the serve never pumps the carrier and the host drops the Link. Plain
/// [`run`] passes a no-op, so host runs and tests are unaffected.
///
/// **The callback SEES the machine** (`&Vm`), which is what lets a serve stream a program's output
/// while it runs rather than only when it ends. Without it the callback could pump the carrier and
/// had nothing to put on it: the output existed, the poll happened, and the two could not meet.
///
/// # Errors
/// Propagates a [`Trap`] from any thread's execution.
pub fn run_serviced(
    module: &Module,
    vm: &mut Vm,
    entry: MethodId,
    args: Vec<Value>,
    service: &mut dyn FnMut(&Vm),
) -> Result<Option<Value>, Trap> {
    match run_interruptible(module, vm, entry, args, &mut |vm| {
        service(vm);
        true
    })? {
        Ran::Finished(value) => Ok(value),
        Ran::Interrupted => Ok(None),
    }
}

/// A thread operation a program raised, in the shape a SINGLE-SESSION driver can act on.
///
/// A [`Session`] only ADVANCES instructions. The threading intrinsics do not block: they record a
/// request on the [`Vm`] for the scheduler to act on, and a driver that steps a session without
/// asking for that request drops every one of them -- silently, because the program simply carries
/// on as if the call had returned. That is what a `Thread.Sleep` under the debugger's resume was
/// doing: not a mis-measured delay, no delay at all.
///
/// A single-session driver has no thread set, so it cannot honor everything the scheduler can. This
/// splits what it CAN do from what it cannot, so the second class is REPORTED rather than dropped:
/// "this tier cannot start a thread" is a fact a user can act on, and silence is not.
pub enum PendingOp {
    /// Nothing was raised.
    None,
    /// `Thread.Sleep`: this session must not advance again until this monotonic-millisecond
    /// deadline, already resolved against the [`Vm`]'s clock.
    SleepUntil(u64),
    /// A cooperative yield with nobody to yield to -- carry on.
    Yield,
    /// An operation that needs the thread set only the scheduler owns, named for the caller to
    /// report.
    NeedsScheduler(&'static str),
}

/// Take the pending thread operation in single-session form -- see [`PendingOp`].
///
/// A `Thread.Sleep` on a machine with no clock seam becomes [`PendingOp::Yield`], which is the same
/// degradation the scheduler makes (it computes a deadline of 0, immediately wakeable): a board with
/// no clock cannot wait, and pretending otherwise here would only move where it fails to.
pub fn take_pending_op(vm: &mut Vm) -> PendingOp {
    let Some(op) = vm.take_thread_op() else {
        return PendingOp::None;
    };
    match op {
        ThreadOp::SleepFor(millis) => match vm.now_millis() {
            Some(now) => PendingOp::SleepUntil(now.saturating_add(millis)),
            None => PendingOp::Yield,
        },
        ThreadOp::SleepForever => PendingOp::SleepUntil(u64::MAX),
        ThreadOp::Yield => PendingOp::Yield,
        ThreadOp::WakeThread { .. } => PendingOp::Yield,
        ThreadOp::Spawn { .. } => PendingOp::NeedsScheduler("Thread.Start"),
        ThreadOp::Join { .. } => PendingOp::NeedsScheduler("Thread.Join"),
        ThreadOp::BlockOnLock { .. } => PendingOp::NeedsScheduler("a contended Monitor lock"),
        ThreadOp::BlockOnIo { .. } => PendingOp::NeedsScheduler("a blocking socket operation"),
    }
}

/// How an [`run_interruptible`] run ended.
pub enum Ran {
    /// The program ran to completion, with the entry's result.
    Finished(Option<Value>),
    /// The service callback asked to stop before the program finished.
    Interrupted,
}

/// [`run_serviced`] whose `service` callback can also STOP the run by answering `false`.
///
/// It exists because a device that DEPLOYS a program must be able to take it back. The firmware
/// pumps its carrier in this callback anyway; letting the same call say "stop" is what makes a
/// deployed run interruptible without the firmware having to drive the interpreter itself -- which
/// is what stepping a bare [`Session`] in bursts amounts to, and a bare session runs no
/// scheduler at all: every thread operation the program raised (`Sleep`, `Start`, `Join`, a
/// contended `Monitor`) was left on the [`Vm`] and silently dropped.
///
/// The callback is also fired around the idle block, so a program that is merely SLEEPING keeps its
/// carrier pumped and stays reclaimable.
///
/// It gets the machine MUTABLY, so a serve that streams the program's output can let go of what it
/// has sent ([`Vm::discard_output`]) -- the one thing that keeps a program that prints for as long as
/// it runs from holding a copy of everything it printed. It runs between instructions, where no
/// session is part-way through one.
///
/// # Errors
/// Propagates a [`Trap`] from any thread's execution.
pub fn run_interruptible(
    module: &Module,
    vm: &mut Vm,
    entry: MethodId,
    args: Vec<Value>,
    service: &mut dyn FnMut(&mut Vm) -> bool,
) -> Result<Ran, Trap> {
    if module.is_baked()
        && !module.initializes_types_on_first_access()
        && !vm.all_cctors_run(module)
        && !module.static_ctors().contains(&entry)
    {
        return Err(Trap::StaticCtorsNotRun);
    }
    let mut threads = Threads::with_main(Session::new(module, entry, args)?);
    threads.schedule::<false>(module, vm, service, &mut Watching::default())
}

/// The program's threads under the cooperative scheduler, kept between calls: what
/// [`run_interruptible`] runs to the end in one call, held so a debugger can run the program a slice
/// at a time ([`Threads::debug_run`]) and look at each thread between slices.
///
/// The entry is thread 0, the main thread, and a `Thread.Start` adds a thread as a fresh [`Session`]
/// (a thread IS a reified session). The scheduler gives the threads turns: a thread runs until it
/// finishes, reaches a yield point (`Thread.Yield` / `Sleep` / `Join`, a contended lock, a blocking
/// socket, signalled through the [`Vm`]'s thread-op channel) or spends its quantum, then the next
/// runnable thread runs. With a single thread -- the common case, and every program that spawns none
/// -- that reduces to running thread 0 to completion.
///
/// A debug run stops a thread part-way through its turn and, when called again, goes on with that
/// same turn, so a program's threads interleave under the debugger exactly as they do without it.
pub struct Threads {
    slots: Vec<ThreadSlot>,
    /// Where the scheduler looks for the next runnable thread: the one taking a turn, while it does.
    cursor: usize,
    /// The number of live (not-finished) threads, mirrored to the Vm so a GC safepoint knows whether
    /// to collect inline (one thread) or defer to the all-threads collection (many).
    live: u32,
    /// The quantum the thread at `cursor` has left, while a debug run has stopped it part-way through
    /// its turn.
    suspended: Option<u32>,
    /// The thread that runs its next instruction although a breakpoint is set on it
    /// ([`Threads::step_off`]).
    stepping_off: Option<u32>,
}

/// What a debug run of the threads stops for, besides the program's end ([`Threads::debug_run`]).
#[derive(Clone, Copy, Debug)]
pub struct Watch {
    /// Stop once this thread has run one instruction: a debugger's single step. That instruction runs
    /// although a breakpoint is set on it, as a step moves off whatever instruction it is on. Every
    /// other thread runs meanwhile, as on .NET, and stops at a breakpoint. `None` runs the threads
    /// until one reaches a breakpoint.
    pub step: Option<u32>,
    /// The most instructions the threads together run before the slice ends.
    pub budget: u32,
    /// The machine-clock time (monotonic milliseconds) the slice ends at, an idle wait included, on a
    /// machine with a clock. Without one, a slice ends at its budget or after an idle wait.
    pub until: Option<u64>,
}

/// Why a debug run's slice ended ([`Threads::debug_run`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Slice {
    /// The slice spent its budget or reached its time with no thread stopped: the program runs on
    /// when the run is called again.
    Running,
    /// Thread `id` has reached an instruction with an enabled breakpoint, which it has not run.
    Breakpoint(u32),
    /// The thread the slice watched ([`Watch::step`]) has run its one instruction.
    Stepped(u32),
    /// No thread can run and nothing will wake one: every thread left is asleep for good or blocked
    /// on another. The program has not ended, as on .NET; only its debugger can end it now.
    Waiting,
    /// Every foreground thread has finished: the main thread's return.
    Finished(Option<Value>),
}

impl Threads {
    /// The threads of a program started at `entry` with `args`: the main thread alone, about to run
    /// `entry`'s first instruction.
    ///
    /// # Errors
    /// Returns [`Trap::NoSuchMethod`] if `entry` is not a managed method.
    pub fn new(module: &Module, entry: MethodId, args: Vec<Value>) -> Result<Threads, Trap> {
        Ok(Threads::with_main(Session::new(module, entry, args)?))
    }

    /// The threads of a program whose main thread is `session`, the only one, about to run.
    fn with_main(session: Session) -> Threads {
        Threads {
            slots: alloc::vec![ThreadSlot {
                id: 0,
                session,
                state: ThreadState::Ready,
                result: None,
                background: false,
            }],
            cursor: 0,
            live: 1,
            suspended: None,
            stepping_off: None,
        }
    }

    /// The thread `id` names, while it has not finished.
    fn live_slot(&self, id: u32) -> Option<&ThreadSlot> {
        self.slots
            .iter()
            .find(|slot| slot.id == id && slot.state != ThreadState::Done)
    }

    /// The threads that have not finished, by id, in the order they started: the main thread, 0,
    /// first while it runs.
    #[must_use]
    pub fn ids(&self) -> Vec<u32> {
        self.slots
            .iter()
            .filter(|slot| slot.state != ThreadState::Done)
            .map(|slot| slot.id)
            .collect()
    }

    /// Thread `id`'s session -- its call stack -- while it has not finished.
    #[must_use]
    pub fn session(&self, id: u32) -> Option<&Session> {
        self.live_slot(id).map(|slot| &slot.session)
    }

    /// [`Threads::session`], to change: a debugger writing a variable.
    pub fn session_mut(&mut self, id: u32) -> Option<&mut Session> {
        self.slots
            .iter_mut()
            .find(|slot| slot.id == id && slot.state != ThreadState::Done)
            .map(|slot| &mut slot.session)
    }

    /// The name thread `id`'s program gave it (`Thread.Name`), while it runs and has one.
    ///
    /// Read from the `System.Threading.Thread` whose id is this thread's, found on the heap at the
    /// slots the loader recorded ([`Module::thread_slots`]). Nothing is kept alive to name a thread
    /// by, so a debugged program's collections are a run's: a `Thread` the program no longer holds
    /// has no name once a collection has reclaimed it. `None` for the main thread, an unnamed thread,
    /// or a module whose corlib records no slots.
    #[must_use]
    pub fn thread_name(&self, module: &Module, vm: &Vm, id: u32) -> Option<String> {
        if id == 0 {
            return None;
        }
        self.live_slot(id)?;
        let (id_slot, name_slot) = module.thread_slots()?;
        let wanted = Value::Int32(i32::try_from(id).ok()?);
        let heap = vm.heap();
        let thread = (0..)
            .map(ObjectRef)
            .map_while(|reference| Some((reference, heap.get(reference)?)))
            .find(|(_, object)| {
                matches!(object, crate::object::Object::Instance { type_id, fields }
                    if fields.get(id_slot as usize) == Some(&wanted)
                        && module.type_full_name(*type_id) == Some("System.Threading.Thread"))
            })?
            .0;
        let Some(Value::Object(name)) = heap.instance_field(thread, name_slot) else {
            return None;
        };
        Some(String::from_utf16_lossy(&heap.as_string(name)?))
    }

    /// Whether thread `id` is waiting inside a call -- asleep, joining another thread, blocked on a
    /// lock or a socket -- rather than about to run its next instruction. Its innermost frame is then
    /// past the call it waits in.
    #[must_use]
    pub fn is_waiting(&self, id: u32) -> bool {
        self.live_slot(id)
            .is_some_and(|slot| slot.state != ThreadState::Ready)
    }

    /// The thread the scheduler gave the last turn to, while it has not finished; otherwise the
    /// first that has not. `None` once every thread has finished.
    #[must_use]
    pub fn current(&self) -> Option<u32> {
        self.slots
            .get(self.cursor)
            .filter(|slot| slot.state != ThreadState::Done)
            .or_else(|| self.slots.iter().find(|slot| slot.state != ThreadState::Done))
            .map(|slot| slot.id)
    }

    /// Lets thread `id` run its next instruction although a breakpoint is set on it: the thread a
    /// debugger reported stopped at that breakpoint, going on. Without this a continue would stop on
    /// the same breakpoint again at once.
    pub fn step_off(&mut self, id: u32) {
        self.stepping_off = Some(id);
    }

    /// Sets the breakpoints every thread stops at, replacing any set before: each a method and the
    /// instruction a thread stops before, in the session's ip domain. A thread started later holds
    /// them too: it takes the set of the thread that started it.
    pub fn set_breakpoints(&mut self, breakpoints: &[(MethodId, u32)]) {
        for slot in &mut self.slots {
            slot.session.clear_breakpoints();
            for &(method, instruction) in breakpoints {
                slot.session.add_breakpoint(method, instruction);
            }
        }
    }

    /// Runs the threads for one slice of a debug session: until `watch` is met, a thread reaches a
    /// breakpoint, the slice ends or the program does. See [`Slice`].
    ///
    /// The threads run as [`run_interruptible`] runs them, turn by turn, with no service callback;
    /// a thread stopped part-way through its turn finishes that turn first when the run is called
    /// again.
    ///
    /// # Errors
    /// Propagates a [`Trap`] from any thread's execution, which ends the program.
    pub fn debug_run(&mut self, module: &Module, vm: &mut Vm, watch: &Watch) -> Result<Slice, Trap> {
        let mut watching = Watching {
            step: watch.step,
            budget: watch.budget,
            until: watch.until,
            ..Watching::default()
        };
        Ok(match self.schedule::<true>(module, vm, &mut |_| true, &mut watching)? {
            Ran::Finished(result) => Slice::Finished(result),
            Ran::Interrupted => match watching.ended {
                Some(Ended::Running) | None => Slice::Running,
                Some(Ended::Breakpoint(id)) => Slice::Breakpoint(id),
                Some(Ended::Stepped(id)) => Slice::Stepped(id),
                Some(Ended::Waiting) => Slice::Waiting,
            },
        })
    }

    /// The scheduler: gives the threads turns until every foreground thread has finished or
    /// `service` stops the run, as [`run_interruptible`] runs them -- or, in a debug run, until the
    /// slice ends ([`Threads::debug_run`]).
    ///
    /// `DEBUG` is a debug run's: it may stop a thread part-way through its turn and takes that turn up
    /// first when called again, bounds its idle waits, watches for breakpoints and its step, and ends
    /// its slice for the reason it leaves in `watching`. A run is the same code without those: none of
    /// them is compiled into it, so a device's firmware carries none.
    fn schedule<const DEBUG: bool>(
        &mut self,
        module: &Module,
        vm: &mut Vm,
        service: &mut dyn FnMut(&mut Vm) -> bool,
        watching: &mut Watching,
    ) -> Result<Ran, Trap> {
        loop {
            let resumed = if DEBUG { self.suspended.take() } else { None };
            let (index, mut quantum) = if let Some(quantum) = resumed {
                (self.cursor, quantum)
            } else {
                if self
                    .slots
                    .iter()
                    .all(|slot| slot.background || slot.state == ThreadState::Done)
                {
                    break;
                }
                drain_pin_events(module, vm, &mut self.slots)?;
                let Some(index) = next_ready_thread(&self.slots, self.cursor) else {
                    if !service(vm) {
                        return Ok(Ran::Interrupted);
                    }
                    if idle_wait(&mut self.slots, vm, if DEBUG { watching.until } else { None }) {
                        if !service(vm) {
                            return Ok(Ran::Interrupted);
                        }
                        if DEBUG && (!watching.timed(vm) || watching.past(vm)) {
                            return Ok(watching.end(Ended::Running));
                        }
                        continue;
                    }
                    if self
                        .slots
                        .iter()
                        .any(|slot| slot.state == ThreadState::SleepingForever)
                    {
                        if DEBUG {
                            return Ok(watching.end(Ended::Waiting));
                        }
                        vm.sleep_millis(FOREVER_IDLE_SLICE_MS);
                        continue;
                    }
                    if DEBUG {
                        return Ok(watching.end(Ended::Waiting));
                    }
                    break;
                };
                self.cursor = index;
                (index, TIME_SLICE_QUANTUM)
            };
            let id = self.slots[index].id;
            vm.set_current_thread(id);
            vm.set_live_thread_count(self.live);
            if DEBUG {
                watching.watched = watching.step == Some(id) && !watching.stepped;
                watching.stepping_off = self.stepping_off == Some(id);
            }
            let status = self.slots[index]
                .session
                .run_until_yield::<DEBUG>(module, vm, service, &mut quantum, watching)?;
            if DEBUG && self.stepping_off == Some(id) && !watching.stepping_off {
                self.stepping_off = None;
            }
            match status {
                ThreadStatus::Interrupted => {
                    if DEBUG && let Some(why) = watching.suspension.take() {
                        self.suspended = Some(quantum);
                        return Ok(watching.end(match why {
                            Suspension::Breakpoint => Ended::Breakpoint(id),
                            Suspension::Stepped => Ended::Stepped(id),
                            Suspension::Spent => Ended::Running,
                        }));
                    }
                    return Ok(Ran::Interrupted);
                }
                ThreadStatus::Done(value) => {
                    let finished = self.slots[index].id;
                    self.slots[index].result = Some(value);
                    self.slots[index].state = ThreadState::Done;
                    vm.mark_thread_finished(finished);
                    self.live -= 1;
                    for slot in &mut self.slots {
                        if matches!(slot.state, ThreadState::Joining(target, _) if target == finished) {
                            slot.state = ThreadState::Ready;
                        }
                    }
                    self.cursor = index + 1;
                }
                ThreadStatus::Yielded => match vm.take_thread_op() {
                    Some(ThreadOp::Spawn { id, method, target, background }) => {
                        let mut session =
                            Session::new(module, method, delegate_call_args(&target, &[]))?;
                        if DEBUG {
                            session.breakpoints = self.slots[index].session.breakpoints.clone();
                        }
                        self.slots.push(ThreadSlot {
                            id,
                            session,
                            state: ThreadState::Ready,
                            result: None,
                            background,
                        });
                        self.live += 1;
                    }
                    Some(ThreadOp::Yield) => self.cursor = index + 1,
                    Some(ThreadOp::Join { target, deadline }) => {
                        let pending = self
                            .slots
                            .iter()
                            .any(|slot| slot.id == target && slot.state != ThreadState::Done);
                        if pending {
                            self.slots[index].state = ThreadState::Joining(target, deadline);
                        }
                        self.cursor = index + 1;
                    }
                    Some(ThreadOp::BlockOnLock { wake, deadline }) => {
                        self.slots[index].state = match deadline {
                            Some(deadline) => ThreadState::BlockedUntil(deadline),
                            None => ThreadState::Blocked,
                        };
                        if let Some(id) = wake {
                            for slot in &mut self.slots {
                                if slot.id == id && is_lock_blocked(&slot.state) {
                                    slot.state = ThreadState::Ready;
                                }
                            }
                        }
                        self.cursor = index + 1;
                    }
                    Some(ThreadOp::WakeThread { id }) => {
                        for slot in &mut self.slots {
                            if slot.id == id && is_lock_blocked(&slot.state) {
                                slot.state = ThreadState::Ready;
                            }
                        }
                        self.cursor = index + 1;
                    }
                    Some(ThreadOp::SleepFor(millis)) => {
                        let deadline = vm.now_millis().map_or(0, |now| now.saturating_add(millis));
                        self.slots[index].state = ThreadState::Sleeping(deadline);
                        self.cursor = index + 1;
                    }
                    Some(ThreadOp::SleepForever) => {
                        self.slots[index].state = ThreadState::SleepingForever;
                        self.cursor = index + 1;
                    }
                    Some(ThreadOp::BlockOnIo { socket, interest, deadline }) => {
                        if let Some(backend) = vm.net_backend() {
                            backend.register(socket, interest);
                        }
                        self.slots[index].state = ThreadState::IoWait(socket, deadline);
                        self.cursor = index + 1;
                    }
                    None => self.cursor = index + 1,
                },
            }
            #[cfg(feature = "gc")]
            if vm.take_force_collect() {
                collect_all_threads(&mut self.slots, module, vm);
            }
            if DEBUG {
                if let (true, Some(id)) = (watching.stepped, watching.step) {
                    return Ok(watching.end(Ended::Stepped(id)));
                }
                if watching.past(vm) {
                    return Ok(watching.end(Ended::Running));
                }
            }
        }
        Ok(Ran::Finished(
            self.slots
                .first_mut()
                .and_then(|slot| slot.result.take())
                .flatten(),
        ))
    }
}

/// A steppable, inspectable execution: the foundation the debugger drives.
///
/// A `Session` owns the call stack and advances one CIL instruction at a time,
/// which is what instruction-level (source-free) debugging needs. It can run to
/// completion ([`Session::run`]), single-step ([`Session::step`]), or resume until
/// a breakpoint ([`Session::resume`]); the call stack is open to inspection
/// throughout ([`Session::frame`]). The module and the runtime context ([`Vm`]) are
/// passed in on each call rather than owned -- the session borrows neither, so a
/// debugger can own the module it steps, and the heap and console outlive a session.
pub struct Session {
    frames: Vec<Frame>,
    /// Instruction breakpoints keyed by `(method, instruction-index)`, each mapped to
    /// whether it is enabled. A disabled breakpoint is remembered (so a debugger can
    /// toggle it back on) but does not pause execution. A `BTreeMap` rather than a set so
    /// enable/disable need not re-send the address.
    breakpoints: BTreeMap<(MethodId, u32), bool>,
    result: Option<Option<Value>>,
    /// When set, an exception that unwinds off the bottom of the call stack pauses the
    /// session ([`StopReason::Exception`]) with the thrown object reported, rather than
    /// ending the run with [`Trap::UnhandledException`]. Off by default, so
    /// [`Session::run`]/[`Session::resume`]/[`Session::step`] keep their abort-on-unhandled
    /// behaviour; a debugger opts in via [`Session::set_pause_on_unhandled_exception`].
    #[cfg(feature = "exceptions")]
    pause_on_exception: bool,
    /// The exception object an unhandled-exception pause parked on, so
    /// [`Session::stopped_exception`] can report it. The call stack has unwound by then (an
    /// unhandled exception is terminal), so the object -- not the frames -- is the
    /// inspectable artifact. Cleared when the run advances again; always `None` without
    /// `pause_on_exception`.
    #[cfg(feature = "exceptions")]
    unhandled_exception: Option<ObjectRef>,
}

/// A code location: a method and the index of an instruction within it -- the unit a
/// breakpoint addresses and a [`Stop`] reports. This is the interpreter-side ("CIL
/// offset") location the device-agnostic debug seam carries; a driver maps it to source
/// (via a PDB) or to a wire address as it sees fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeLocation {
    /// The method the location is in.
    pub method: MethodId,
    /// The index (not byte offset) of the instruction within the method.
    pub instruction: u32,
}

/// Why a [`Session`] run loop ([`Session::continue_`], [`Session::step_into`],
/// [`Session::step_over`], [`Session::step_out`]) stopped.
///
/// This is the device-agnostic stop seam: the on-device Lamella Link stub and the host
/// `lamella-dap` backend both read it. It is the [`Session`]-level counterpart of the
/// host adapter's `Stop`, kept in the no_std core so a device driver shares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Paused at the program's first instruction (the explicit stop-at-entry a debugger
    /// requests before any code runs). Reserved for a driver that begins paused; the run
    /// loops here never originate it (they report `Breakpoint` / `Step` / `Exited`).
    EntryPoint,
    /// Paused before an enabled breakpoint's instruction.
    Breakpoint,
    /// A single-step (into/over/out) completed.
    Step,
    /// The program ran to completion. The [`Stop`]'s `location` is then `None` and its
    /// `returned` holds the entry method's result.
    Exited,
    /// An exception unwound off the bottom of the call stack and the session was
    /// configured to pause on it ([`Session::set_pause_on_unhandled_exception`]); the call
    /// stack has unwound, and [`Session::stopped_exception`] gives the thrown object.
    #[cfg(feature = "exceptions")]
    Exception,
}

/// The outcome of a [`Session`] run-loop call: why it stopped, and where (the location of
/// the top frame when it stopped, or `None` once the program has finished and the call
/// stack is empty).
#[derive(Debug, Clone, PartialEq)]
pub struct Stop {
    /// Why the run loop stopped.
    pub reason: StopReason,
    /// The location execution is paused at (the top frame's method + instruction), or
    /// `None` if the program ran to completion.
    pub location: Option<CodeLocation>,
    /// The value the entry method returned, present only when the program ran to
    /// completion (`location` is then `None`).
    pub returned: Option<Value>,
}

/// One named, inspectable value -- a field or element a debugger drilled into via
/// [`Session::expand`]. A driver renders/types the `value` itself (e.g. `lamella-dap`'s
/// `format_value`); the core supplies the name and the raw [`Value`].
#[derive(Debug, Clone)]
pub struct NamedValue {
    /// The display name (`field0` for an instance field by slot, `[2]` for an array
    /// element, `value` for a box's content).
    pub name: String,
    /// The value itself, for display or further expansion.
    pub value: Value,
}

/// Whether `value` can be drilled into by [`Session::expand`] -- an object instance with
/// fields, an array with elements, a box, or an inline value-type instance with fields.
/// A bare scalar cannot.
#[must_use]
fn is_expandable(vm: &Vm, value: &Value) -> bool {
    match value {
        Value::Object(reference) => {
            vm.heap().array_len(*reference).is_some()
                || vm.heap().boxed_value(*reference).is_some()
                || matches!(vm.heap().get(*reference), Some(crate::object::Object::Instance { .. }))
        }
        Value::Struct(fields) => !fields.is_empty(),
        _ => false,
    }
}

/// The state of a [`Session`] after a step or resume.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Execution has not finished; more instructions remain.
    Running,
    /// Execution paused at a breakpoint, before the instruction there ran.
    Paused,
    /// Execution finished; the entry method returned this value.
    Done(Option<Value>),
}

/// A read-only view of one activation frame, for inspection such as a debugger's
/// stack trace and variables.
#[derive(Debug, Clone, Copy)]
pub struct FrameView<'s> {
    /// The method running in this frame.
    pub method: MethodId,
    /// Where execution is in the method, IN THE ACTIVE IP DOMAIN: the INDEX of the instruction
    /// about to execute by default, and its CIL BYTE OFFSET under `code-in-place`.
    ///
    pub ip: u32,
    /// The evaluation stack, bottom first.
    pub stack: &'s [Value],
    /// The local variables.
    pub locals: &'s [Value],
    /// The arguments.
    pub args: &'s [Value],
}

impl Session {
    /// Starts a session at `entry` with `args`. The `module` is used to set up the
    /// first frame but not retained; pass it again to each step/resume/run.
    ///
    /// # Errors
    /// Returns [`Trap::NoSuchMethod`] if `entry` is not a managed method.
    pub fn new(module: &Module, entry: MethodId, args: Vec<Value>) -> Result<Session, Trap> {
        Ok(Session {
            frames: alloc::vec![new_frame_cold(module, entry, args)?],
            breakpoints: BTreeMap::new(),
            result: None,
            #[cfg(feature = "exceptions")]
            pause_on_exception: false,
            #[cfg(feature = "exceptions")]
            unhandled_exception: None,
        })
    }

    /// Sets (and enables) a breakpoint before instruction `instruction` of `method`. Setting
    /// one that already exists re-enables it.
    pub fn add_breakpoint(&mut self, method: MethodId, instruction: u32) {
        self.breakpoints.insert((method, instruction), true);
    }

    /// Clears a breakpoint set by [`Session::add_breakpoint`] (forgotten entirely, unlike
    /// [`Session::set_breakpoint_enabled`] with `false`, which keeps it disabled).
    pub fn remove_breakpoint(&mut self, method: MethodId, instruction: u32) {
        self.breakpoints.remove(&(method, instruction));
    }

    /// Enables or disables the breakpoint at `(method, instruction)`, creating it (in the
    /// requested state) if it does not exist. A disabled breakpoint is remembered but does
    /// not pause execution -- the toggle a DAP `setBreakpoints` with `enabled: false`, or a
    /// Lamella Link `Disable`, maps to.
    pub fn set_breakpoint_enabled(&mut self, method: MethodId, instruction: u32, enabled: bool) {
        self.breakpoints.insert((method, instruction), enabled);
    }

    /// Whether an enabled breakpoint is set at `(method, instruction)`.
    #[must_use]
    pub fn is_breakpoint_enabled(&self, method: MethodId, instruction: u32) -> bool {
        self.breakpoints
            .get(&(method, instruction))
            .copied()
            .unwrap_or(false)
    }

    /// Removes all breakpoints (e.g. when a debugger replaces the whole set).
    pub fn clear_breakpoints(&mut self) {
        self.breakpoints.clear();
    }

    /// Whether the session is currently sitting on an enabled breakpoint -- the next
    /// [`Session::resume`] / [`Session::continue_`] would pause immediately. A debugger uses
    /// this to step off a breakpoint before continuing.
    #[must_use]
    pub fn is_at_breakpoint(&self) -> bool {
        self.at_breakpoint()
    }

    /// Makes an exception that unwinds off the bottom of the call stack pause the session
    /// ([`StopReason::Exception`], the faulting frames still inspectable) instead of ending
    /// the run with [`Trap::UnhandledException`]. Only the new [`Session::continue_`] /
    /// `step_*` loop honours it; [`Session::run`]/[`Session::resume`]/[`Session::step`] keep
    /// aborting, so existing callers (and the differential) are unaffected. No-op without the
    /// `exceptions` feature (nothing throws).
    #[cfg(feature = "exceptions")]
    pub fn set_pause_on_unhandled_exception(&mut self, pause: bool) {
        self.pause_on_exception = pause;
    }

    /// The exception object the session is parked on after a [`StopReason::Exception`] stop,
    /// if any. The faulting frames sit beneath it ([`Session::frame`]), and
    /// [`Session::expand`] reads its fields. `None` at any other stop.
    #[cfg(feature = "exceptions")]
    #[must_use]
    pub fn stopped_exception(&self) -> Option<ObjectRef> {
        self.unhandled_exception
    }

    /// Executes exactly one instruction, ignoring breakpoints.
    ///
    /// # Errors
    /// Returns a [`Trap`] if the instruction faults.
    pub fn step(&mut self, module: &Module, vm: &mut Vm) -> Result<Status, Trap> {
        if let Some(result) = &self.result {
            return Ok(Status::Done(result.clone()));
        }
        self.advance(module, vm)
    }

    /// Runs until a breakpoint is reached or the program finishes. A breakpoint
    /// pauses *before* its instruction runs; to continue past one the session is
    /// sitting on, [`Session::step`] once, then resume again.
    ///
    /// # Errors
    /// Returns a [`Trap`] if an instruction faults.
    pub fn resume(&mut self, module: &Module, vm: &mut Vm) -> Result<Status, Trap> {
        loop {
            if let Some(result) = &self.result {
                return Ok(Status::Done(result.clone()));
            }
            if self.at_breakpoint() {
                return Ok(Status::Paused);
            }
            if let Status::Done(value) = self.advance(module, vm)? {
                return Ok(Status::Done(value));
            }
        }
    }

    /// Runs to completion, ignoring breakpoints, returning the entry's result.
    ///
    /// # Errors
    /// Returns a [`Trap`] if an instruction faults.
    pub fn run(&mut self, module: &Module, vm: &mut Vm) -> Result<Option<Value>, Trap> {
        loop {
            if let Some(result) = &self.result {
                return Ok(result.clone());
            }
            self.advance(module, vm)?;
        }
    }

    /// Runs to completion OR until a threading intrinsic raises a scheduler request (the cooperative
    /// yield points: `Thread.Start` / `Join` / `Yield` / `Sleep`), whichever comes first. The
    /// scheduler ([`Threads`]) then acts on the request and resumes a (possibly different) thread.
    /// With no threading this never yields and is exactly [`Session::run`].
    ///
    /// `quantum` is the turn's time slice, counted down here. `DEBUG` is a debug run's
    /// ([`Threads::debug_run`]): the thread then also stops part-way through its turn -- before an
    /// instruction with a breakpoint, once the watched thread has run its one instruction, or when the
    /// run's budget or time is spent -- leaving `quantum` where it got to. A run pays nothing for it.
    ///
    /// # Errors
    /// Returns a [`Trap`] if an instruction faults.
    fn run_until_yield<const DEBUG: bool>(
        &mut self,
        module: &Module,
        vm: &mut Vm,
        service: &mut dyn FnMut(&mut Vm) -> bool,
        quantum: &mut u32,
        watching: &mut Watching,
    ) -> Result<ThreadStatus, Trap> {
        loop {
            if let Some(result) = &self.result {
                return Ok(ThreadStatus::Done(result.clone()));
            }
            if DEBUG {
                if watching.budget == 0 {
                    return Ok(watching.suspend(Suspension::Spent));
                }
                if !watching.watched && !watching.stepping_off && self.at_breakpoint() {
                    return Ok(watching.suspend(Suspension::Breakpoint));
                }
                watching.budget -= 1;
            }
            self.advance(module, vm)?;
            if DEBUG {
                watching.stepping_off = false;
                if watching.watched {
                    watching.watched = false;
                    watching.stepped = true;
                }
                if let Some(result) = &self.result {
                    return Ok(ThreadStatus::Done(result.clone()));
                }
            }
            if vm.has_thread_op() {
                return Ok(ThreadStatus::Yielded);
            }
            *quantum -= 1;
            if *quantum == 0 {
                if !service(vm) {
                    return Ok(ThreadStatus::Interrupted);
                }
                let _ = vm.now_millis();
                if vm.live_thread_count() > 1 {
                    return Ok(ThreadStatus::Yielded);
                }
                *quantum = TIME_SLICE_QUANTUM;
                if DEBUG && watching.past(vm) {
                    return Ok(watching.suspend(Suspension::Spent));
                }
            }
            if DEBUG && watching.stepped {
                return Ok(watching.suspend(Suspension::Stepped));
            }
        }
    }

    /// The number of frames on the call stack (0 once finished).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// A view of frame `index`, with 0 the outermost (entry) frame and the last
    /// the innermost (currently executing).
    #[must_use]
    pub fn frame(&self, index: usize) -> Option<FrameView<'_>> {
        let frame = self.frames.get(index)?;
        Some(FrameView {
            method: frame.method,
            ip: frame.ip as u32,
            stack: &frame.stack,
            locals: &frame.locals,
            args: &frame.args,
        })
    }

    /// Writes `value` into local-variable slot `slot` of frame `index` (the write companion to
    /// [`frame`](Session::frame)'s read view -- the debugger's `setVariable`, editing a local
    /// mid-debug). Returns `true` if written, `false` if the frame index or slot is out of range. The
    /// caller is responsible for supplying a value of the slot's existing kind.
    pub fn set_local(&mut self, index: usize, slot: usize, value: Value) -> bool {
        match self.frames.get_mut(index) {
            Some(frame) if slot < frame.locals.len() => {
                frame.locals[slot] = value;
                true
            }
            _ => false,
        }
    }

    /// Writes `value` into argument slot `slot` of frame `index` -- like [`set_local`](Session::set_local)
    /// but for the frame's arguments. Returns `true` if written, `false` if out of range.
    pub fn set_arg(&mut self, index: usize, slot: usize, value: Value) -> bool {
        match self.frames.get_mut(index) {
            Some(frame) if slot < frame.args.len() => {
                frame.args[slot] = value;
                true
            }
            _ => false,
        }
    }

    fn at_breakpoint(&self) -> bool {
        if self.breakpoints.is_empty() {
            return false;
        }
        match self.frames.last() {
            Some(frame) => {
                self.breakpoints
                    .get(&(frame.method, frame.ip as u32))
                    .copied()
                    == Some(true)
            }
            None => false,
        }
    }

    /// The location execution is paused at -- the top (innermost) frame's method and
    /// instruction index -- or `None` once the program has finished. This is the location a
    /// [`Stop`] reports, exposed on its own so a driver can read "where am I" without a step.
    #[must_use]
    pub fn location(&self) -> Option<CodeLocation> {
        self.frames.last().map(|frame| CodeLocation {
            method: frame.method,
            instruction: frame.ip as u32,
        })
    }

    /// The `this` reference of the innermost frame -- argument slot 0 when `module` records
    /// that method's first argument as `this` (i.e. it is an instance method) -- or `None`
    /// for a static method (or when no debug names are loaded). A debugger surfaces this
    /// distinctly from the other arguments.
    #[must_use]
    pub fn this(&self, module: &Module) -> Option<Value> {
        let frame = self.frames.last()?;
        module
            .arg_name(frame.method, 0)
            .filter(|name| *name == "this")
            .and_then(|_| frame.args.first())
            .cloned()
    }

    /// Resumes until an enabled breakpoint, an unhandled exception
    /// ([`Session::set_pause_on_unhandled_exception`]), or completion -- the device-agnostic
    /// "continue" the Lamella Link and `lamella-dap` both drive. Unlike [`Session::resume`], it
    /// reports *why* it stopped and *where* via a [`Stop`], and (when enabled) pauses on an
    /// unhandled exception rather than trapping. It steps off a breakpoint it is already
    /// sitting on before running, so a repeated `continue_` makes progress.
    ///
    /// # Errors
    /// Returns a [`Trap`] if an instruction faults in a way that is not a catchable,
    /// pausable exception (malformed CIL, stack overflow, an unresolved token, ...).
    pub fn continue_(&mut self, module: &Module, vm: &mut Vm) -> Result<Stop, Trap> {
        if let Some(result) = &self.result {
            return Ok(self.done(result.clone()));
        }
        if self.at_breakpoint() {
            if let Some(stop) = self.tick(module, vm)? {
                return Ok(stop);
            }
        }
        loop {
            if self.at_breakpoint() {
                return Ok(self.stop(StopReason::Breakpoint));
            }
            if let Some(stop) = self.tick(module, vm)? {
                return Ok(stop);
            }
        }
    }

    /// Single-steps one instruction, descending into a call -- "step into". Stops at the
    /// next instruction wherever it lands (the callee's first instruction across a call, or
    /// the next instruction in this frame), or earlier on an enabled breakpoint / a pausable
    /// unhandled exception.
    ///
    /// # Errors
    /// Returns a [`Trap`] as [`Session::continue_`] does.
    pub fn step_into(&mut self, module: &Module, vm: &mut Vm) -> Result<Stop, Trap> {
        if let Some(result) = &self.result {
            return Ok(self.done(result.clone()));
        }
        match self.tick(module, vm)? {
            Some(stop) => Ok(stop),
            None if self.at_breakpoint() => Ok(self.stop(StopReason::Breakpoint)),
            None => Ok(self.stop(StopReason::Step)),
        }
    }

    /// Steps over a call -- "step over" / DAP `next`. A call at the current instruction runs
    /// to completion (back to this frame or a caller) before stopping; otherwise it behaves
    /// like [`Session::step_into`]. An enabled breakpoint hit inside the stepped-over call
    /// takes priority and pauses there.
    ///
    /// # Errors
    /// Returns a [`Trap`] as [`Session::continue_`] does.
    pub fn step_over(&mut self, module: &Module, vm: &mut Vm) -> Result<Stop, Trap> {
        let start = self.depth();
        self.step_until(module, vm, |session| session.depth() <= start)
    }

    /// Steps out of the current method -- "step out" / DAP `stepOut`. Runs until the current
    /// method returns (the call stack is shallower than it is now), then stops at the next
    /// instruction in the caller, or earlier on an enabled breakpoint / a pausable unhandled
    /// exception.
    ///
    /// # Errors
    /// Returns a [`Trap`] as [`Session::continue_`] does.
    pub fn step_out(&mut self, module: &Module, vm: &mut Vm) -> Result<Stop, Trap> {
        let start = self.depth();
        self.step_until(module, vm, |session| session.depth() < start)
    }

    /// Single-steps until `reached` holds (after a step that completes), a breakpoint is hit,
    /// an exception pauses, or the program finishes -- the shared engine of `step_over` /
    /// `step_out`.
    fn step_until(
        &mut self,
        module: &Module,
        vm: &mut Vm,
        reached: impl Fn(&Session) -> bool,
    ) -> Result<Stop, Trap> {
        if let Some(result) = &self.result {
            return Ok(self.done(result.clone()));
        }
        loop {
            if let Some(stop) = self.tick(module, vm)? {
                return Ok(stop);
            }
            if self.at_breakpoint() {
                return Ok(self.stop(StopReason::Breakpoint));
            }
            if reached(self) {
                return Ok(self.stop(StopReason::Step));
            }
        }
    }

    /// Advances exactly one instruction for the stepping/continue loop, mapping its outcome
    /// to an early-stop [`Stop`] (completion, or -- when armed -- a pausable unhandled
    /// exception) or `None` to keep going. A breakpoint is *not* detected here (the callers
    /// check `at_breakpoint` at the right boundary); this only runs the instruction.
    fn tick(&mut self, module: &Module, vm: &mut Vm) -> Result<Option<Stop>, Trap> {
        #[cfg(feature = "exceptions")]
        {
            self.unhandled_exception = None;
        }
        let outcome = self.advance(module, vm);
        match outcome {
            Ok(Status::Done(value)) => {
                #[cfg(feature = "exceptions")]
                let _ = vm.take_unhandled();
                Ok(Some(self.done(value)))
            }
            Ok(Status::Running | Status::Paused) => {
                #[cfg(feature = "exceptions")]
                let _ = vm.take_unhandled();
                Ok(None)
            }
            #[cfg(feature = "exceptions")]
            Err(Trap::UnhandledException(_)) if self.pause_on_exception => {
                self.unhandled_exception = vm.take_unhandled();
                Ok(Some(self.stop(StopReason::Exception)))
            }
            Err(trap) => Err(trap),
        }
    }

    /// Builds a [`Stop`] for stopping (still executing) at the current top-frame location.
    fn stop(&self, reason: StopReason) -> Stop {
        Stop {
            reason,
            location: self.location(),
            returned: None,
        }
    }

    /// Builds the completion [`Stop`] (no location; the call stack is empty).
    fn done(&self, returned: Option<Value>) -> Stop {
        Stop {
            reason: StopReason::Exited,
            location: None,
            returned,
        }
    }

    /// Expands an inspectable `value` into its constituents for a debugger's variable tree:
    /// an object instance's fields (`fieldN` by slot), an array's elements (`[i]`), a
    /// box's single inner value, or an INLINE value-type instance's fields (`fieldN` -- a
    /// struct local/argument is a [`Value::Struct`], not a heap reference). A scalar
    /// expands to nothing. `vm` owns the heap the references point into. This is the read
    /// side of variable drill-down the Lamella Link and the DAP `variables` request both use.
    /// (Instance field *names* live in metadata a driver can layer on; the core names them
    /// by slot.)
    #[must_use]
    pub fn expand(&self, vm: &Vm, value: &Value) -> Vec<NamedValue> {
        if let Value::Struct(fields) = value {
            return fields
                .iter()
                .enumerate()
                .map(|(slot, value)| NamedValue {
                    name: alloc::format!("field{slot}"),
                    value: value.clone(),
                })
                .collect();
        }
        let Value::Object(reference) = value else {
            return Vec::new();
        };
        let reference = *reference;
        if let Some(len) = vm.heap().array_len(reference) {
            return (0..len)
                .filter_map(|index| {
                    vm.heap().array_get(reference, index).map(|value| NamedValue {
                        name: alloc::format!("[{index}]"),
                        value,
                    })
                })
                .collect();
        }
        if let Some(inner) = vm.heap().boxed_value(reference) {
            return alloc::vec![NamedValue {
                name: "value".to_string(),
                value: inner,
            }];
        }
        if let Some(crate::object::Object::Instance { fields, .. }) = vm.heap().get(reference) {
            return fields
                .iter()
                .enumerate()
                .map(|(slot, value)| NamedValue {
                    name: alloc::format!("field{slot}"),
                    value: value.clone(),
                })
                .collect();
        }
        Vec::new()
    }

    /// Whether `value` can be drilled into with [`Session::expand`] (so a driver knows to
    /// offer it as expandable). `vm` owns the heap.
    #[must_use]
    pub fn is_expandable(&self, vm: &Vm, value: &Value) -> bool {
        is_expandable(vm, value)
    }

    /// The GC safepoint at an instruction boundary, where every frame's state is consistent: collects
    /// when a collection was requested, or the heap has reached its threshold or its object budget.
    /// [`Session::advance`] calls it only while [`Heap::collection_check_pending`] is set, and it
    /// clears that first; what it does here re-arms it where the triggers must be weighed again.
    #[cfg(feature = "gc")]
    fn gc_safepoint(&mut self, module: &Module, vm: &mut Vm) {
        vm.heap_mut().clear_collection_check();
        let forced = vm.take_force_collect();
        if vm.collection_suspended() {
            if forced {
                vm.request_collect();
            }
        } else if forced || vm.heap().should_collect() || vm.heap().at_budget() {
            if vm.live_thread_count() <= 1 {
                self.collect_garbage(module, vm);
            } else {
                vm.request_collect();
            }
        }
    }

    /// Executes one instruction of the top frame and applies its effect to the
    /// call stack: a return pops (handing the result back, or finishing); a call
    /// pushes a managed frame or invokes an intrinsic.
    fn advance(&mut self, module: &Module, vm: &mut Vm) -> Result<Status, Trap> {
        #[cfg(feature = "gc")]
        if vm.heap().collection_check_pending() {
            self.gc_safepoint(module, vm);
        }
        let Session { frames, result, .. } = self;
        let current = match frames.len() {
            0 => return Ok(Status::Done(None)),
            len => len - 1,
        };
        let (callers, current_frame) = frames.split_at_mut(current);
        let top = &mut current_frame[0];
        let asm = top.context.asm;
        #[cfg(feature = "code-in-place")]
        let code = match top.context.code {
            Some(code) => code,
            None => method_code(module, top.method)?,
        };
        #[cfg(not(feature = "code-in-place"))]
        let code = method_code(module, top.method)?;
        let instruction_ip = top.ip;
        top.fault_ip = instruction_ip;
        #[cfg(not(feature = "code-in-place"))]
        let instruction = {
            let fetched = code.get(top.ip).ok_or(Trap::FellThroughEnd)?;
            top.ip += 1;
            fetched
        };
        #[cfg(feature = "code-in-place")]
        let fetched_in_place;
        #[cfg(feature = "code-in-place")]
        let instruction = {
            let (decoded, next_ip) =
                lamella_cil::decode_at(code, top.ip).map_err(|_| Trap::FellThroughEnd)?;
            fetched_in_place = decoded;
            top.ip = next_ip;
            &fetched_in_place
        };
        let flow = match step(top, current, callers, code.len(), Some(module), vm, instruction) {
            Ok(flow) => flow,
            #[cfg(feature = "exceptions")]
            Err(trap) => match catchable_fault(&trap, module, vm, Some(top.method)) {
                Some(exception) => Flow::Throw(exception),
                None => return Err(trap),
            },
            #[cfg(not(feature = "exceptions"))]
            Err(trap) => return Err(trap),
        };
        match flow {
            Flow::Next => Ok(Status::Running),
            Flow::Return(value) => {
                let mut returned = frames.pop();
                if let Some((rest, params)) = returned.as_mut().and_then(|f| f.multicast.take()) {
                    if let Some(((target, method), remaining)) = rest.split_first() {
                        let call_args = delegate_call_args(target, &params);
                        let (method, remaining) = (*method, remaining.to_vec());
                        if let Some(finished) = returned {
                            vm.frame_pool.retire(finished);
                        }
                        if frames.len() >= MAX_CALL_DEPTH {
                            return Err(Trap::CallStackOverflow);
                        }
                        let mut next = new_frame(vm, module, method, call_args)?;
                        next.multicast = Some((remaining, params));
                        frames.push(next);
                        push_pending_cctor(frames, module, vm, method)?;
                        return Ok(Status::Running);
                    }
                }
                let returned_object = returned.as_ref().and_then(|frame| frame.new_object);
                let returned_value_location = returned.as_mut().and_then(|frame| frame.new_value.take());
                if let Some(finished) = returned {
                    vm.frame_pool.retire(finished);
                }
                let returned_struct = returned_value_location
                    .map(|location| read_byref(frames, vm, location, Opcode::Newobj))
                    .transpose()?;
                match frames.last_mut() {
                    Some(caller) => {
                        if let Some(object) = returned_object {
                            caller.stack.push(Value::Object(object));
                        } else if let Some(structure) = returned_struct {
                            caller.stack.push(structure);
                        } else if let Some(value) = value {
                            caller.stack.push(value);
                        }
                        Ok(Status::Running)
                    }
                    None => {
                        *result = Some(value.clone());
                        Ok(Status::Done(value))
                    }
                }
            }
            Flow::Jmp(target) => {
                let mut replaced = frames.pop().ok_or(Trap::StackUnderflow)?;
                let args = core::mem::take(&mut replaced.args);
                vm.frame_pool.retire(replaced);
                let frame = new_frame(vm, module, target, args)?;
                frames.push(frame);
                push_pending_cctor(frames, module, vm, target)?;
                Ok(Status::Running)
            }
            Flow::Call { method, args } => match module.method_kind(method) {
                Some(MethodKind::Managed) => {
                    if frames.len() >= MAX_CALL_DEPTH {
                        return Err(Trap::CallStackOverflow);
                    }
                    let frame = new_frame(vm, module, method, args)?;
                    frames.push(frame);
                    push_pending_cctor(frames, module, vm, method)?;
                    Ok(Status::Running)
                }
                Some(MethodKind::Intrinsic(func)) => {
                    let mut args = args;
                    run_pending_cctor_nested(module, vm, method, frames, &mut args)?;
                    if !is_compare_exchange(module, method) {
                        read_builtin_receiver(frames, vm, module, method, &mut args)?;
                        deref_byref_args_in_place(frames, vm, &mut args)?;
                    }
                    #[cfg(feature = "gc")]
                    vm.park_frames(core::mem::take(frames));
                    let outcome = func(vm, module, &args);
                    #[cfg(feature = "gc")]
                    {
                        *frames = vm.unpark_frames();
                    }
                    vm.frame_pool.give_values(args);
                    match outcome {
                        Ok(result) => {
                            if let Some(value) = result {
                                frames
                                    .last_mut()
                                    .ok_or(Trap::CallStackOverflow)?
                                    .stack
                                    .push(value);
                            }
                            Ok(Status::Running)
                        }
                        #[cfg(feature = "exceptions")]
                        Err(trap) => match catchable_fault(
                            &trap,
                            module,
                            vm,
                            frames.last().map(|frame| frame.method),
                        ) {
                            Some(exception) => {
                                vm.note_unhandled(exception);
                                raise(frames, module, vm, exception)
                            }
                            None => Err(trap),
                        },
                        #[cfg(not(feature = "exceptions"))]
                        Err(trap) => Err(trap),
                    }
                }
                None => Err(Trap::NoSuchMethod(method)),
            },
            Flow::CallVararg {
                method,
                args,
                vararg_start,
                var_types,
            } => {
                if frames.len() >= MAX_CALL_DEPTH {
                    return Err(Trap::CallStackOverflow);
                }
                let mut frame = new_frame(vm, module, method, args)?;
                frame.varargs = Some(VarargFrame {
                    start: vararg_start,
                    types: var_types,
                });
                frames.push(frame);
                push_pending_cctor(frames, module, vm, method)?;
                Ok(Status::Running)
            }
            Flow::NewObj { ctor, object, args } => match module.method_kind(ctor) {
                Some(MethodKind::Managed) => {
                    if frames.len() >= MAX_CALL_DEPTH {
                        return Err(Trap::CallStackOverflow);
                    }
                    let mut args = args;
                    let mut full_args = vm.frame_pool.take_values();
                    full_args.push(Value::Object(object));
                    full_args.append(&mut args);
                    vm.frame_pool.give_values(args);
                    let mut frame = new_frame(vm, module, ctor, full_args)?;
                    frame.new_object = Some(object);
                    frames.push(frame);
                    push_pending_cctor(frames, module, vm, ctor)?;
                    Ok(Status::Running)
                }
                Some(MethodKind::Intrinsic(func)) => {
                    let mut args = args;
                    let mut full_args = vm.frame_pool.take_values();
                    full_args.push(Value::Object(object));
                    full_args.append(&mut args);
                    vm.frame_pool.give_values(args);
                    #[cfg(feature = "gc")]
                    vm.park_frames(core::mem::take(frames));
                    let outcome = func(vm, module, &full_args);
                    #[cfg(feature = "gc")]
                    {
                        *frames = vm.unpark_frames();
                    }
                    vm.frame_pool.give_values(full_args);
                    outcome?;
                    frames
                        .last_mut()
                        .ok_or(Trap::CallStackOverflow)?
                        .stack
                        .push(Value::Object(object));
                    Ok(Status::Running)
                }
                None => Err(Trap::NoSuchMethod(ctor)),
            },
            Flow::NewValueObj {
                ctor,
                location,
                args,
            } => match module.method_kind(ctor) {
                Some(MethodKind::Managed) => {
                    if frames.len() >= MAX_CALL_DEPTH {
                        return Err(Trap::CallStackOverflow);
                    }
                    let mut args = args;
                    let mut full_args = vm.frame_pool.take_values();
                    full_args.push(Value::ByRef(location.clone()));
                    full_args.append(&mut args);
                    vm.frame_pool.give_values(args);
                    let mut frame = new_frame(vm, module, ctor, full_args)?;
                    frame.new_value = Some(location);
                    frames.push(frame);
                    push_pending_cctor(frames, module, vm, ctor)?;
                    Ok(Status::Running)
                }
                Some(MethodKind::Intrinsic(_)) => {
                    vm.frame_pool.give_values(args);
                    let value = read_byref(frames, vm, location, Opcode::Newobj)?;
                    frames
                        .last_mut()
                        .ok_or(Trap::CallStackOverflow)?
                        .stack
                        .push(value);
                    Ok(Status::Running)
                }
                None => Err(Trap::NoSuchMethod(ctor)),
            },
            Flow::NewString {
                pointer,
                start,
                length,
            } => match build_string_from_pointer(frames, vm, pointer, start, length) {
                Ok(reference) => {
                    frames
                        .last_mut()
                        .ok_or(Trap::CallStackOverflow)?
                        .stack
                        .push(Value::Object(reference));
                    Ok(Status::Running)
                }
                #[cfg(feature = "exceptions")]
                Err(trap) => match catchable_fault(
                    &trap,
                    module,
                    vm,
                    frames.last().map(|frame| frame.method),
                ) {
                    Some(exception) => {
                        vm.note_unhandled(exception);
                        raise(frames, module, vm, exception)
                    }
                    None => Err(trap),
                },
                #[cfg(not(feature = "exceptions"))]
                Err(trap) => Err(trap),
            },
            Flow::EnsureCctor(cctor) => {
                if let Some(top) = frames.last_mut() {
                    top.ip = instruction_ip;
                }
                if frames.len() >= MAX_CALL_DEPTH {
                    return Err(Trap::CallStackOverflow);
                }
                let frame = new_frame(vm, module, cctor, Vec::new())?;
                frames.push(frame);
                Ok(Status::Running)
            }
            Flow::Throw(exception) => {
                #[cfg(feature = "exceptions")]
                vm.note_unhandled(exception);
                raise(frames, module, vm, exception)
            }
            Flow::Leave(target) => {
                let finallys = {
                    let method = frames.last().ok_or(Trap::StackUnderflow)?.method;
                    let handlers = method_handlers(module, method)?;
                    let frame = frames.last_mut().ok_or(Trap::StackUnderflow)?;
                    frame.stack.clear();
                    let leave_ip = frame.ip.saturating_sub(1);
                    finallys_exited(&handlers, leave_ip, target)
                };
                begin_finallys(frames, module, vm, finallys, AfterFinally::Goto(target))
            }
            Flow::EndFinally => {
                let pending = frames
                    .last_mut()
                    .ok_or(Trap::StackUnderflow)?
                    .pending
                    .pop();
                match pending {
                    Some(PendingFinally { finallys, then, .. }) => {
                        begin_finallys(frames, module, vm, finallys, then)
                    }
                    None => Err(Trap::Unsupported(Opcode::Endfinally)),
                }
            }
            Flow::EndFilter(accept) => {
                let pending = frames
                    .last_mut()
                    .ok_or(Trap::StackUnderflow)?
                    .pending_filter
                    .take();
                match pending {
                    Some(filter) if accept => {
                        let method = frames.last().ok_or(Trap::StackUnderflow)?.method;
                        let handlers = method_handlers(module, method)?;
                        abandon_finallys_for(
                            frames.last_mut().ok_or(Trap::StackUnderflow)?,
                            filter.handler,
                        );
                        let finallys =
                            finallys_inside(&handlers, filter.fault_ip, filter.filter_try);
                        begin_finallys(
                            frames,
                            module,
                            vm,
                            finallys,
                            AfterFinally::Catch {
                                handler: filter.handler,
                                exception: filter.exception,
                            },
                        )
                    }
                    Some(filter) => raise_from(
                        frames,
                        module,
                        vm,
                        filter.exception,
                        filter.resume,
                        filter.fault_ip,
                    ),
                    None => Err(Trap::Unsupported(Opcode::Endfilter)),
                }
            }
            Flow::LoadField { location, field } => {
                let slot = module
                    .field_slot(asm, field)
                    .ok_or_else(|| unresolved_field(module, asm, field))?;
                let value = if matches!(location, Location::Stack { .. }) {
                    let (address, storage) = stack_field_address(module, asm, field, slot, location, Opcode::Ldfld)?;
                    let bytes = stack_field_bytes(frames, module, &address, storage, Opcode::Ldfld)?;
                    decode_stack_field(module, storage, bytes, 0)?
                } else if let Location::Element { array, index, byte_offset } = location
                    && array_element_index(vm, array, index, byte_offset).is_none()
                {
                    return Err(Trap::Unsupported(Opcode::Ldfld));
                } else if moved_off_a_whole_value(vm, &location)
                    && !matches!(read_location_value(frames, vm, location.clone()), Some(Value::Struct(_)))
                {
                    return Err(Trap::Unsupported(Opcode::Ldfld));
                } else {
                    read_field_at(frames, vm, location, slot)
                };
                frames
                    .get_mut(current)
                    .ok_or(Trap::StackUnderflow)?
                    .stack
                    .push(value);
                Ok(Status::Running)
            }
            Flow::StoreField {
                location,
                field,
                value,
            } => {
                let slot = module
                    .field_slot(asm, field)
                    .ok_or_else(|| unresolved_field(module, asm, field))?;
                if matches!(location, Location::Stack { .. }) {
                    let (address, storage) = stack_field_address(module, asm, field, slot, location, Opcode::Stfld)?;
                    let mut bytes = stack_field_bytes(frames, module, &address, storage, Opcode::Stfld)?.to_vec();
                    encode_stack_field(module, storage, &mut bytes, &value, 0)?;
                    let Location::Stack { frame, buffer, offset } = address else {
                        return Err(Trap::TypeMismatch(Opcode::Stfld));
                    };
                    let destination = frames.get_mut(frame)
                        .and_then(|frame| frame.buffers.get_mut(buffer))
                        .and_then(|buffer| buffer.get_mut(offset as usize..(offset as usize).checked_add(bytes.len())?))
                        .ok_or(Trap::NullReference)?;
                    destination.copy_from_slice(&bytes);
                    return Ok(Status::Running);
                }
                let type_id = module
                    .field_type(asm, field)
                    .ok_or_else(|| unresolved_field(module, asm, field))?;
                let shape = module
                    .type_field_defaults(type_id)
                    .ok_or_else(|| unresolved_field(module, asm, field))?;
                write_field_at(frames, vm, location, slot, &shape, value)?;
                Ok(Status::Running)
            }
            Flow::InitObj { location, kind } => {
                let value = match declared_variable_kind(frames, module, &location) {
                    // A local or an argument of a recorded number kind zeroes as that kind (ECMA-335 6th ed
                    // III.4.5 and I.12.1.1.1): `initobj int32*` leaves the null pointer.
                    Some(declared) if declared_slot_width(declared, crate::native_pointer_size() as usize).is_some() => {
                        zero_of_slot_kind(declared)
                    }
                    _ => zero_of_type(module, module.type_id_of(asm, kind), asm_key(asm, kind.0)),
                };
                write_location_value(frames, vm, location, value)?;
                Ok(Status::Running)
            }
            Flow::LoadObj { location, opcode, token } => {
                let (opcode, structure) = obj_access(frames, vm, module, asm, &location, opcode, token);
                let value = match structure {
                    Some(id) => load_struct_from_bytes(frames, vm, module, &location, id)?,
                    None => typed_load(frames, vm, module, location, opcode)?,
                };
                frames
                    .get_mut(current)
                    .ok_or(Trap::StackUnderflow)?
                    .stack
                    .push(value);
                Ok(Status::Running)
            }
            Flow::StoreObj { location, value, opcode, token } => {
                let (store, structure) = obj_access(frames, vm, module, asm, &location, opcode, token);
                if let Some(id) = structure {
                    store_struct_into_bytes(frames, vm, module, &location, id, &value)?;
                    return Ok(Status::Running);
                }
                // A `stobj` that moves as a `stind` takes its value as that `stind` does in `step`.
                let value = match (opcode, store) {
                    (Opcode::Stobj, Opcode::StindI1 | Opcode::StindI2) => narrow_stored_int(value, store),
                    (Opcode::Stobj, Opcode::StindR4 | Opcode::StindR8) => stored_at_instruction_width(value, store)?,
                    _ => value,
                };
                if !store_into_declared_slot(frames, module, &location, store, &value)?
                    && !store_into_packed_element(vm, &location, store, &value)?
                    && !store_into_struct_variable(frames, module, &location, store, &value)?
                    && !store_into_object_fields(vm, module, &location, store, &value)?
                {
                    let value = checked_whole_store(frames, vm, module, &location, store, stored_storage(module, asm, store, token), value)?;
                    let value = stored_at_slot_kind(declared_variable_kind(frames, module, &location), value);
                    write_location_value(frames, vm, location, value)?;
                }
                Ok(Status::Running)
            }
            Flow::CopyObj { dest, src } => {
                let value = read_byref(frames, vm, src, Opcode::Cpobj)?;
                let value = stored_at_slot_kind(declared_variable_kind(frames, module, &dest), value);
                write_location_value(frames, vm, dest, value)?;
                Ok(Status::Running)
            }
            Flow::LoadStack {
                owner,
                buffer,
                offset,
                opcode,
            } => {
                let width = indirect_width(opcode).ok_or(Trap::TypeMismatch(opcode))?;
                let raw = frames
                    .get(owner)
                    .and_then(|frame| frame.read_stack(buffer, offset, width))
                    .ok_or(Trap::NullReference)?;
                let value = stack_loaded_value(opcode, raw)?;
                frames
                    .get_mut(current)
                    .ok_or(Trap::StackUnderflow)?
                    .stack
                    .push(value);
                Ok(Status::Running)
            }
            Flow::StoreStack {
                owner,
                buffer,
                offset,
                opcode,
                value,
            } => {
                let frame = frames.get_mut(owner).ok_or(Trap::NullReference)?;
                let location = Location::Stack {
                    frame: owner,
                    buffer,
                    offset,
                };
                store_stack_indirect(frame, owner, opcode, location, value)?;
                Ok(Status::Running)
            }
            Flow::InvokeMulticast {
                mut invocations,
                params,
            } => {
                if invocations.is_empty() {
                    return Ok(Status::Running);
                }
                let (target, method) = invocations.remove(0);
                let call_args = delegate_receiver_args(module, vm, method, &target, &params);
                if frames.len() >= MAX_CALL_DEPTH {
                    return Err(Trap::CallStackOverflow);
                }
                let mut frame = new_frame(vm, module, method, call_args)?;
                frame.multicast = Some((invocations, params));
                frames.push(frame);
                Ok(Status::Running)
            }
        }
    }
}

/// The array element index a `Location::Element` addresses as a whole element: its base `index` moved
/// by the `byte_offset` a pinned-array pointer accumulated, forward or back ([`element_displacement`];
/// `fixed (int* p = arr)` then `p[i]`, ECMA-334 2nd ed 25.6). With a zero offset (a plain `ldelema`
/// pointer) this is the base index. `None` where the pointer was moved by a part of an element, or to
/// before the first element or past any index: no element starts there, and a typed access through an
/// array whose elements have bytes of their own is answered from those bytes before this is asked.
/// `None` too for a moved pointer into an array whose element width is not known here (a rectangular
/// array of structs): which element it reaches is not known, so none is answered for it.
fn array_element_index(vm: &Vm, array: ObjectRef, index: usize, byte_offset: u32) -> Option<usize> {
    if byte_offset == 0 {
        return Some(index);
    }
    let width = i64::try_from(vm.heap().array_element_width(array)?).ok().filter(|&width| width != 0)?;
    let displacement = element_displacement(byte_offset);
    if displacement % width != 0 {
        return None;
    }
    usize::try_from(i64::try_from(index).ok()?.checked_add(displacement / width)?).ok()
}

/// The UTF-16 code unit at `index` of a string's units, treating the position one past the
/// last unit as the U+0000 terminator after every pinned string's characters (ECMA-334 2nd ed
/// 25.6: the pointer value `p + s.Length` of a fixed string always points to a null character), so
/// `fixed (char* p = "")` reads '\0' and a terminator scan stops at the end; anything further is
/// outside the pinned data.
fn string_unit_at(units: &[u16], index: usize) -> Option<u16> {
    units
        .get(index)
        .copied()
        .or_else(|| (index == units.len()).then_some(0))
}

/// The byte at `offset` of a string's character data as a pinned pointer reaches it: its UTF-16 code
/// units, each in the target's byte order (little-endian here, ECMA-335 6th ed I.12.6.3), then the
/// U+0000 terminator .NET keeps after every string ([`string_unit_at`]). `None` past the terminator.
fn string_byte_at(units: &[u16], offset: usize) -> Option<u8> {
    string_unit_at(units, offset / 2).map(|unit| unit.to_le_bytes()[offset % 2])
}

/// Reads with the `ldind.<type>` `opcode` through a pointer into a pinned string's character data
/// ([`Location::StringChar`]): the instruction loads a value of its type from the address (ECMA-335 6th
/// ed III.3.42), so it reads that many bytes of the code units from the pointer on ([`string_byte_at`]),
/// inside one unit or across several: `((byte*)p)[1]` of `fixed (char* p = "\u0201")` reads 2, and
/// `*(int*)p` of "AB" 0x00420041 (ECMA-334 2nd ed 25.4 and 25.6). A read past the terminator is
/// refused. `None` for any other location and for an instruction with no width here.
fn load_from_string_chars(vm: &Vm, location: &Location, opcode: Opcode) -> Result<Option<Value>, Trap> {
    let Location::StringChar { string, byte_offset } = *location else {
        return Ok(None);
    };
    let native = crate::native_pointer_size() as usize;
    let Some(width) = indirect_width_at(opcode, native) else {
        return Ok(None);
    };
    let units = vm.heap().as_string(string).ok_or(Trap::NullReference)?;
    let mut raw = [0u8; 8];
    for (index, byte) in raw.iter_mut().take(width).enumerate() {
        *byte = string_byte_at(&units, (byte_offset as usize).checked_add(index).ok_or(Trap::Unsupported(opcode))?)
            .ok_or(Trap::Unsupported(opcode))?;
    }
    stack_loaded_value_at(opcode, raw, native).map(Some)
}

/// The whole value at a managed-pointer `location` -- a frame local/arg, a heap
/// object's field, an array element, or a static field -- if present.
fn read_location_value(frames: &[Frame], vm: &Vm, location: Location) -> Option<Value> {
    match location {
        Location::Local { frame, slot } => {
            frames.get(frame).and_then(|f| f.locals.get(slot)).cloned()
        }
        Location::Arg { frame, slot } => frames.get(frame).and_then(|f| f.args.get(slot)).cloned(),
        Location::Field { object, slot } => vm.heap().instance_field(object, slot),
        Location::Element {
            array,
            index,
            byte_offset,
        } => array_element_index(vm, array, index, byte_offset).and_then(|element| vm.heap().array_get(array, element)),
        Location::StringChar {
            string,
            byte_offset,
        } => vm
            .heap()
            .as_string(string)
            .and_then(|units| string_unit_at(&units, byte_offset as usize / 2))
            .map(|unit| Value::Int32(i32::from(unit))),
        Location::Static { slot } => vm.static_field(slot),
        Location::Boxed { object } => vm.heap().boxed_value(object),
        Location::Nested { base, slot } => match read_location_value(frames, vm, (*base).clone()) {
            Some(Value::Struct(fields)) => fields.get(slot as usize).cloned(),
            _ => None,
        },
        Location::Stack { .. } => None,
        Location::LocalBytes {
            frame,
            slot,
            byte_offset: 0,
        } => read_location_value(frames, vm, Location::Local { frame, slot }),
        Location::ArgBytes {
            frame,
            slot,
            byte_offset: 0,
        } => read_location_value(frames, vm, Location::Arg { frame, slot }),
        Location::StaticBytes { slot, byte_offset: 0 } => read_location_value(frames, vm, Location::Static { slot }),
        Location::LocalBytes { .. } | Location::ArgBytes { .. } | Location::StaticBytes { .. } => None,
    }
}

/// Writes the whole `value` at a managed-pointer `location`.
fn write_location_value(
    frames: &mut [Frame],
    vm: &mut Vm,
    location: Location,
    value: Value,
) -> Result<(), Trap> {
    match location {
        Location::Local { frame, slot } => {
            let frame = frames
                .get_mut(frame)
                .ok_or(Trap::Unsupported(Opcode::Stobj))?;
            set_slot(&mut frame.locals, slot, value);
            Ok(())
        }
        Location::Arg { frame, slot } => {
            let frame = frames
                .get_mut(frame)
                .ok_or(Trap::Unsupported(Opcode::Stobj))?;
            set_slot(&mut frame.args, slot, value);
            Ok(())
        }
        Location::Field { object, slot } => vm
            .heap_mut()
            .set_instance_field(object, slot, value)
            .then_some(())
            .ok_or(Trap::NullReference),
        Location::Element {
            array,
            index,
            byte_offset,
        } => {
            let element = array_element_index(vm, array, index, byte_offset).ok_or(Trap::Unsupported(Opcode::Stobj))?;
            vm.heap_mut()
                .array_set(array, element, value)
                .then_some(())
                .ok_or(Trap::IndexOutOfRange(element as i32))
        }
        Location::Static { slot } => {
            vm.set_static_field(slot, value);
            Ok(())
        }
        Location::Boxed { object } => vm
            .heap_mut()
            .set_boxed_value(object, value)
            .then_some(())
            .ok_or(Trap::NullReference),
        Location::Nested { base, slot } => {
            let mut fields = match read_location_value(frames, vm, (*base).clone()) {
                Some(Value::Struct(fields)) => fields,
                _ => return Err(Trap::Unsupported(Opcode::Stfld)),
            };
            if let Some(target) = fields.get_mut(slot as usize) {
                *target = value;
            }
            write_location_value(frames, vm, *base, Value::Struct(fields))
        }
        Location::Stack { .. } => Err(Trap::Unsupported(Opcode::Stobj)),
        Location::StringChar { .. } => Err(Trap::Unsupported(Opcode::Stobj)),
        Location::LocalBytes {
            frame,
            slot,
            byte_offset: 0,
        } => write_location_value(frames, vm, Location::Local { frame, slot }, value),
        Location::ArgBytes {
            frame,
            slot,
            byte_offset: 0,
        } => write_location_value(frames, vm, Location::Arg { frame, slot }, value),
        Location::StaticBytes { slot, byte_offset: 0 } => write_location_value(frames, vm, Location::Static { slot }, value),
        Location::LocalBytes { .. } | Location::ArgBytes { .. } | Location::StaticBytes { .. } => {
            Err(Trap::Unsupported(Opcode::Stobj))
        }
    }
}

/// Whether `location` is a place a pointer was moved to where no whole value starts, or a field of one:
/// an array element no element index names ([`array_element_index`]), or a variable or a static at a
/// byte past its start.
fn moved_off_a_whole_value(vm: &Vm, location: &Location) -> bool {
    match location {
        Location::Element { array, index, byte_offset } => array_element_index(vm, *array, *index, *byte_offset).is_none(),
        Location::LocalBytes { byte_offset, .. } | Location::ArgBytes { byte_offset, .. } | Location::StaticBytes { byte_offset, .. } => {
            *byte_offset != 0
        }
        Location::Nested { base, .. } => moved_off_a_whole_value(vm, base),
        _ => false,
    }
}

/// Reads field `slot` of the value-type instance at `location` (zero if the slot has
/// not been materialized into a struct yet -- an `init_locals` zero).
fn read_field_at(frames: &[Frame], vm: &Vm, location: Location, slot: u32) -> Value {
    match read_location_value(frames, vm, location) {
        Some(Value::Struct(fields)) => fields
            .get(slot as usize)
            .cloned()
            .unwrap_or(Value::Int32(0)),
        _ => Value::Int32(0),
    }
}

fn stack_field_address(
    module: &Module,
    asm: u8,
    field: Token,
    slot: u32,
    base: Location,
    opcode: Opcode,
) -> Result<(Location, crate::module::FieldStorage), Trap> {
    let owner = module.field_type(asm, field).ok_or_else(|| unresolved_field(module, asm, field))?;
    let layout = module.type_field_offsets(owner).ok_or(Trap::Unsupported(opcode))?;
    if slot as usize >= layout.slot_count() {
        return Err(Trap::FieldSlotMissing { field, slot });
    }
    let displacement = layout.offset_of(slot as usize).ok_or(Trap::Unsupported(opcode))?;
    let storage = module.type_field_storage(owner, slot as usize).ok_or(Trap::Unsupported(opcode))?;
    let width = stack_field_width(module, storage, opcode)?;
    if displacement.checked_add(width).is_none_or(|end| end > layout.end()) {
        return Err(Trap::Unsupported(opcode));
    }
    let Location::Stack { frame, buffer, offset } = base else {
        return Err(Trap::TypeMismatch(opcode));
    };
    Ok((Location::Stack {
        frame, buffer, offset: offset.checked_add(displacement).ok_or(Trap::NullReference)?,
    }, storage))
}

fn stack_field_width(module: &Module, storage: crate::module::FieldStorage, opcode: Opcode) -> Result<u32, Trap> {
    use crate::module::FieldStorage;
    match storage {
        FieldStorage::Primitive(kind) if kind.packable() => Ok(kind.byte_width() as u32),
        FieldStorage::Native => Ok(crate::native_pointer_size()),
        FieldStorage::ValueType(id) => module.type_field_offsets(id).map(|layout| layout.end()).ok_or(Trap::Unsupported(opcode)),
        _ => Err(Trap::Unsupported(opcode)),
    }
}

fn stack_field_bytes<'a>(
    frames: &'a [Frame], module: &Module, address: &Location,
    storage: crate::module::FieldStorage, opcode: Opcode,
) -> Result<&'a [u8], Trap> {
    let Location::Stack { frame, buffer, offset } = address else {
        return Err(Trap::TypeMismatch(opcode));
    };
    let width = stack_field_width(module, storage, opcode)? as usize;
    let start = *offset as usize;
    frames.get(*frame).and_then(|frame| frame.buffers.get(*buffer))
        .and_then(|buffer| buffer.get(start..start.checked_add(width)?))
        .ok_or(Trap::NullReference)
}

fn stack_field_opcodes(storage: crate::module::FieldStorage) -> Option<(Opcode, Opcode)> {
    use crate::{module::FieldStorage, PrimKind};
    Some(match storage {
        FieldStorage::Primitive(kind) => match kind {
            PrimKind::I1 => (Opcode::LdindI1, Opcode::StindI1),
            PrimKind::U1 => (Opcode::LdindU1, Opcode::StindI1),
            PrimKind::I2 => (Opcode::LdindI2, Opcode::StindI2),
            PrimKind::U2 => (Opcode::LdindU2, Opcode::StindI2),
            PrimKind::I4 => (Opcode::LdindI4, Opcode::StindI4),
            PrimKind::I8 => (Opcode::LdindI8, Opcode::StindI8),
            PrimKind::F4 => (Opcode::LdindR4, Opcode::StindR4),
            PrimKind::F8 => (Opcode::LdindR8, Opcode::StindR8),
        },
        FieldStorage::Native => (Opcode::LdindI, Opcode::StindI),
        _ => return None,
    })
}

// Recursive metadata cannot expand the Rust call stack without bound.
const MAX_STACK_FIELD_DEPTH: usize = 64;

fn decode_stack_field(
    module: &Module, storage: crate::module::FieldStorage, bytes: &[u8], depth: usize,
) -> Result<Value, Trap> {
    use crate::module::FieldStorage;
    if depth >= MAX_STACK_FIELD_DEPTH {
        return Err(Trap::Unsupported(Opcode::Ldfld));
    }
    if let Some((load, _)) = stack_field_opcodes(storage) {
        let mut raw = [0; 8];
        let destination = raw.get_mut(..bytes.len()).ok_or(Trap::Unsupported(Opcode::Ldfld))?;
        destination.copy_from_slice(bytes);
        return stack_loaded_value(load, raw).map_err(|_| Trap::Unsupported(Opcode::Ldfld));
    }
    let FieldStorage::ValueType(id) = storage else { return Err(Trap::Unsupported(Opcode::Ldfld)) };
    let layout = module.type_field_offsets(id).ok_or(Trap::Unsupported(Opcode::Ldfld))?;
    let mut fields = Vec::new();
    for slot in 0..layout.slot_count() {
        let field = module.type_field_storage(id, slot).ok_or(Trap::Unsupported(Opcode::Ldfld))?;
        let at = layout.offset_of(slot).ok_or(Trap::Unsupported(Opcode::Ldfld))? as usize;
        let width = stack_field_width(module, field, Opcode::Ldfld)? as usize;
        let end = at.checked_add(width).ok_or(Trap::Unsupported(Opcode::Ldfld))?;
        let bytes = bytes.get(at..end).ok_or(Trap::Unsupported(Opcode::Ldfld))?;
        fields.push(decode_stack_field(module, field, bytes, depth + 1)?);
    }
    if module.type_handle_of(id).is_some_and(|handle| module.is_enum_by_handle(handle)) {
        if fields.len() != 1 { return Err(Trap::Unsupported(Opcode::Ldfld)) }
        return Ok(fields.remove(0));
    }
    Ok(Value::Struct(fields.into_boxed_slice()))
}

fn encode_stack_field(
    module: &Module, storage: crate::module::FieldStorage, bytes: &mut [u8], value: &Value, depth: usize,
) -> Result<(), Trap> {
    use crate::module::FieldStorage;
    if depth >= MAX_STACK_FIELD_DEPTH {
        return Err(Trap::Unsupported(Opcode::Stfld));
    }
    if let Some((_, store)) = stack_field_opcodes(storage) {
        let raw = stack_stored_bytes(store, value.clone())?;
        bytes.copy_from_slice(raw.get(..bytes.len()).ok_or(Trap::Unsupported(Opcode::Stfld))?);
        return Ok(());
    }
    let FieldStorage::ValueType(id) = storage else { return Err(Trap::Unsupported(Opcode::Stfld)) };
    let layout = module.type_field_offsets(id).ok_or(Trap::Unsupported(Opcode::Stfld))?;
    let fields: &[Value] = if module.type_handle_of(id).is_some_and(|handle| module.is_enum_by_handle(handle)) {
        core::slice::from_ref(value)
    } else if let Value::Struct(fields) = value {
        fields
    } else {
        return Err(Trap::TypeMismatch(Opcode::Stfld));
    };
    if fields.len() != layout.slot_count() { return Err(Trap::TypeMismatch(Opcode::Stfld)) }
    for (slot, value) in fields.iter().enumerate() {
        let field = module.type_field_storage(id, slot).ok_or(Trap::Unsupported(Opcode::Stfld))?;
        let at = layout.offset_of(slot).ok_or(Trap::Unsupported(Opcode::Stfld))? as usize;
        let width = stack_field_width(module, field, Opcode::Stfld)? as usize;
        let end = at.checked_add(width).ok_or(Trap::Unsupported(Opcode::Stfld))?;
        let bytes = bytes.get_mut(at..end).ok_or(Trap::Unsupported(Opcode::Stfld))?;
        encode_stack_field(module, field, bytes, value, depth + 1)?;
    }
    Ok(())
}

/// Dereferences any managed-pointer argument to the value it points at, so an intrinsic
/// (which works on values -- e.g. `Int32.ToString` on `&int`) sees the value, not the
/// pointer.
fn deref_byref_args_in_place(frames: &[Frame], vm: &Vm, args: &mut [Value]) -> Result<(), Trap> {
    for arg in args.iter_mut() {
        if let Value::ByRef(location) = arg {
            *arg = read_byref(frames, vm, location.clone(), Opcode::Call)?;
        }
    }
    Ok(())
}

/// Materializes the string a `newobj String(char*[, int, int])` builds: decode the pointer
/// argument, read the code units it reaches, and allocate the heap string. The bounds
/// checks mirror .NET: a negative `startIndex` or `length` is ArgumentOutOfRangeException;
/// a null pointer yields the empty string for the terminator-scanning form and for an
/// empty range, and faults (reading through null) otherwise.
fn build_string_from_pointer(
    frames: &[Frame],
    vm: &mut Vm,
    pointer: Value,
    start: i32,
    length: Option<i32>,
) -> Result<ObjectRef, Trap> {
    let start = usize::try_from(start).map_err(|_| Trap::ArgumentOutOfRange(1))?;
    let length = match length {
        Some(length) => Some(usize::try_from(length).map_err(|_| Trap::ArgumentOutOfRange(2))?),
        None => None,
    };
    let location = match pointer {
        Value::ByRef(location) => location,
        Value::Null | Value::Int32(0) | Value::NativeInt(0) => {
            return match length {
                None | Some(0) => Ok(vm.heap_mut().alloc_text("")),
                Some(_) => Err(Trap::NullReference),
            };
        }
        _ => return Err(Trap::TypeMismatch(Opcode::Newobj)),
    };
    let units = read_utf16_through_pointer(frames, vm, &location, start, length)?;
    Ok(vm.heap_mut().alloc_string(&units)?)
}

/// Reads UTF-16 code units through a raw `char*` -- a managed `location` naming a
/// (possibly cross-frame) stackalloc buffer, a pinned array's elements, or pinned string
/// data -- starting `start` units past the pointer: exactly `length` units, or, for the
/// terminator-scanning `String(char*)` form, up to (not including) the first U+0000.
/// Running out of the underlying allocation traps ([`Trap::NullReference`], the fault the
/// width-typed `ldind` paths report for an out-of-bounds localloc access).
fn read_utf16_through_pointer(
    frames: &[Frame],
    vm: &Vm,
    location: &Location,
    start: usize,
    length: Option<usize>,
) -> Result<Vec<u16>, Trap> {
    match location {
        Location::Stack {
            frame,
            buffer,
            offset,
        } => {
            let owner = frames.get(*frame).ok_or(Trap::NullReference)?;
            collect_units(
                |index| {
                    let at = u32::try_from(*offset as usize + 2 * (start + index)).ok()?;
                    let raw = owner.read_stack(*buffer, at, 2)?;
                    Some(u16::from_le_bytes([raw[0], raw[1]]))
                },
                length,
            )
        }
        Location::Element {
            array,
            index,
            byte_offset,
        } => {
            let base = array_element_index(vm, *array, *index, *byte_offset)
                .and_then(|element| element.checked_add(start))
                .ok_or(Trap::NullReference)?;
            if let Some(length) = length {
                return vm
                    .heap()
                    .array_utf16_units(*array, base, length)
                    .ok_or(Trap::NullReference);
            }
            collect_units(
                |index| match base.checked_add(index).and_then(|at| vm.heap().array_get(*array, at)) {
                    Some(Value::Int32(unit)) => Some(unit as u16),
                    _ => None,
                },
                None,
            )
        }
        Location::StringChar {
            string,
            byte_offset,
        } => {
            let units = vm.heap().as_string(*string).ok_or(Trap::NullReference)?;
            let base = (*byte_offset as usize).checked_add(start.checked_mul(2).ok_or(Trap::NullReference)?).ok_or(Trap::NullReference)?;
            collect_units(
                |index| {
                    let at = base.checked_add(index.checked_mul(2)?)?;
                    Some(u16::from_le_bytes([string_byte_at(&units, at)?, string_byte_at(&units, at.checked_add(1)?)?]))
                },
                length,
            )
        }
        _ => Err(Trap::TypeMismatch(Opcode::Newobj)),
    }
}

/// Collects code units from `unit_at`: exactly `length` units, or -- scanning -- up to the
/// first U+0000. `None` from `unit_at` (out of the allocation) is [`Trap::NullReference`].
fn collect_units(
    unit_at: impl Fn(usize) -> Option<u16>,
    length: Option<usize>,
) -> Result<Vec<u16>, Trap> {
    match length {
        Some(length) => (0..length)
            .map(|index| unit_at(index).ok_or(Trap::NullReference))
            .collect(),
        None => {
            let mut units = Vec::new();
            loop {
                let unit = unit_at(units.len()).ok_or(Trap::NullReference)?;
                if unit == 0 {
                    return Ok(units);
                }
                units.push(unit);
            }
        }
    }
}

/// Normalizes the value `stind.i1` / `stind.i2` stores through a byref to a VALUE slot (a local, an
/// argument, a field, a static -- NOT raw localloc memory, which takes the value's low bytes): a
/// `NativeInt` collapses to the `int32` such a slot holds for a narrower integer (ECMA-335 6th ed
/// I.12.1.2), and any other value is kept as it is. A local or an argument whose declared kind the
/// loader recorded then takes only the instruction's low byte or two, as a store into a 1- or 2-byte
/// location truncates to fit (ECMA-335 6th ed I.12.1.2 and III.3.62), and is read back by that kind
/// ([`store_into_declared_slot`]); any other slot takes the value in place of its whole value.
fn narrow_stored_int(value: Value, opcode: Opcode) -> Value {
    match (opcode, value) {
        (Opcode::StindI1 | Opcode::StindI2, Value::NativeInt(n)) => Value::Int32(n as i32),
        (_, value) => value,
    }
}

/// The value `stind.r4` or `stind.r8` stores through a byref to a VALUE slot: a float32 or a float64,
/// as the instruction names it, a floating value being rounded from its native size to the size the
/// instruction names (ECMA-335 6th ed III.3.62). Raw localloc memory takes the same widths in
/// [`stack_stored_bytes`]. A value that is not floating is refused there and here, as correct CIL
/// stores a value of the pointer's type (ECMA-335 6th ed III.3.62); on a build without floating point
/// both instructions are refused, as ECMA-335 6th ed IV.4.1.1 leaves them unimplemented there. Every
/// other store keeps its value.
fn stored_at_instruction_width(value: Value, opcode: Opcode) -> Result<Value, Trap> {
    match (opcode, value) {
        #[cfg(feature = "float")]
        (Opcode::StindR8, Value::Single(f)) => Ok(Value::Float(f64::from(f))),
        #[cfg(feature = "float")]
        (Opcode::StindR4, Value::Float(f)) => Ok(Value::Single(f as f32)),
        #[cfg(feature = "float")]
        (Opcode::StindR4 | Opcode::StindR8, value @ (Value::Single(_) | Value::Float(_))) => Ok(value),
        #[cfg(feature = "float")]
        (Opcode::StindR4 | Opcode::StindR8, _) => Err(Trap::TypeMismatch(opcode)),
        #[cfg(not(feature = "float"))]
        (Opcode::StindR4 | Opcode::StindR8, _) => Err(Trap::Unsupported(opcode)),
        (_, value) => Ok(value),
    }
}

/// The zero a local of declared kind `kind` starts as: with the localsinit flag a local is 0 of its
/// integer type, 0.0 of its floating type and null for an object reference (ECMA-335 6th ed I.12.4.1
/// and II.13.2), and a native int or unmanaged pointer the native int 0, the null pointer
/// (I.12.1.1.1). A struct, or a kind the loader did not settle, starts as the int32 0 an unwritten
/// slot reads as.
fn zero_of_slot_kind(kind: SlotKind) -> Value {
    match kind {
        SlotKind::Primitive(PrimKind::I8) => Value::Int64(0),
        #[cfg(feature = "float")]
        SlotKind::Primitive(PrimKind::F4) => Value::Single(0.0),
        #[cfg(feature = "float")]
        SlotKind::Primitive(PrimKind::F8) => Value::Float(0.0),
        SlotKind::Native => Value::NativeInt(0),
        SlotKind::Reference | SlotKind::ByRef => Value::Null,
        _ => Value::Int32(0),
    }
}

/// `value` as a local or an argument of declared kind `kind` holds it: a floating value at the slot's
/// floating width, and a null reference in a native int or pointer slot as the native int 0, the null
/// pointer (an unmanaged pointer is a native unsigned int, ECMA-335 6th ed I.12.1.1.1). A local or an
/// argument may hold a floating value wider than its type (I.12.1.3 and III.1.1.1); here it takes
/// its declared width, so the variable's address reads its declared bytes. Any other value is kept.
fn stored_at_slot_kind(kind: Option<SlotKind>, value: Value) -> Value {
    match (kind, value) {
        #[cfg(feature = "float")]
        (Some(SlotKind::Primitive(PrimKind::F8)), Value::Single(f)) => Value::Float(f64::from(f)),
        #[cfg(feature = "float")]
        (Some(SlotKind::Primitive(PrimKind::F4)), Value::Float(f)) => Value::Single(f as f32),
        (Some(SlotKind::Native), Value::Null) => Value::NativeInt(0),
        (_, value) => value,
    }
}

/// The width in bytes of a local or an argument of declared kind `kind`, where the slot holds a number
/// this build can take apart into bytes: a primitive's width, or the target's native width for a native
/// int or pointer (ECMA-335 6th ed I.12.1.1). `None` for any other kind.
fn declared_slot_width(kind: SlotKind, native: usize) -> Option<usize> {
    match kind {
        #[cfg(not(feature = "float"))]
        SlotKind::Primitive(PrimKind::F4 | PrimKind::F8) => None,
        SlotKind::Primitive(kind) => Some(kind.byte_width()),
        SlotKind::Native => Some(native),
        _ => None,
    }
}

/// The bits of `value` held in a slot of declared kind `kind`, little-endian at the kind's width: a
/// narrow integer's low bytes (the slot holds it widened to int32, ECMA-335 6th ed I.12.1.2). `None`
/// when the value is not that kind's representation.
fn declared_slot_bits(kind: SlotKind, value: &Value) -> Option<u64> {
    match (kind, value) {
        (SlotKind::Primitive(PrimKind::I8), Value::Int64(n)) | (SlotKind::Native, Value::NativeInt(n)) => Some(*n as u64),
        (SlotKind::Primitive(PrimKind::I1 | PrimKind::U1 | PrimKind::I2 | PrimKind::U2 | PrimKind::I4), Value::Int32(n)) => {
            Some(u64::from(*n as u32))
        }
        #[cfg(feature = "float")]
        (SlotKind::Primitive(PrimKind::F4), Value::Single(f)) => Some(u64::from(f.to_bits())),
        #[cfg(feature = "float")]
        (SlotKind::Primitive(PrimKind::F8), Value::Float(f)) => Some(f.to_bits()),
        _ => None,
    }
}

/// The value a slot of declared kind `kind` holds for the little-endian `bits` at its width: a
/// narrow integer sign- or zero-extended to int32 as its type says (ECMA-335 6th ed I.12.1.2). A
/// native int of four bytes with the top bit set is refused, as [`stack_loaded_value_at`] refuses it.
fn declared_slot_value(kind: SlotKind, bits: u64, native: usize, opcode: Opcode) -> Result<Value, Trap> {
    Ok(match kind {
        SlotKind::Primitive(PrimKind::I1) => Value::Int32(i32::from(bits as u8 as i8)),
        SlotKind::Primitive(PrimKind::U1) => Value::Int32(i32::from(bits as u8)),
        SlotKind::Primitive(PrimKind::I2) => Value::Int32(i32::from(bits as u16 as i16)),
        SlotKind::Primitive(PrimKind::U2) => Value::Int32(i32::from(bits as u16)),
        SlotKind::Primitive(PrimKind::I4) => Value::Int32(bits as u32 as i32),
        SlotKind::Primitive(PrimKind::I8) => Value::Int64(bits as i64),
        #[cfg(feature = "float")]
        SlotKind::Primitive(PrimKind::F4) => Value::Single(f32::from_bits(bits as u32)),
        #[cfg(feature = "float")]
        SlotKind::Primitive(PrimKind::F8) => Value::Float(f64::from_bits(bits)),
        SlotKind::Native if native == 4 => {
            let word = bits as u32;
            if word & 0x8000_0000 != 0 {
                return Err(Trap::Unsupported(opcode));
            }
            Value::NativeInt(i64::from(word))
        }
        SlotKind::Native => Value::NativeInt(bits as i64),
        _ => return Err(Trap::Unsupported(opcode)),
    })
}

/// Stores `value` with the `stind.<type>` `opcode` through a pointer to a local or an argument whose
/// declared kind is a number ([`declared_slot_width`]), at the pointer's displacement inside the
/// variable: the instruction stores a value of its type at the address (ECMA-335 6th ed III.3.62),
/// so its bytes replace that many of the variable's bytes and the rest are kept --
/// `*(byte*)&value = 0x7f` changes only `value`'s lowest-addressed byte (ECMA-334 2nd ed 25.4), and
/// `*(float*)&bits = 1.0f` leaves `bits` holding the float's bits. A store that would run past the
/// variable is refused. Returns whether it stored: a variable of any other kind, or one whose kinds
/// the loader did not record, is left to the whole-value store.
fn store_into_declared_slot(
    frames: &mut [Frame],
    module: &Module,
    location: &Location,
    opcode: Opcode,
    value: &Value,
) -> Result<bool, Trap> {
    let (frame, slot, argument, displacement) = match *location {
        Location::Local { frame, slot } => (frame, slot, false, 0),
        Location::Arg { frame, slot } => (frame, slot, true, 0),
        Location::LocalBytes { frame, slot, byte_offset } => (frame, slot, false, byte_offset as usize),
        Location::ArgBytes { frame, slot, byte_offset } => (frame, slot, true, byte_offset as usize),
        _ => return Ok(false),
    };
    let native = crate::native_pointer_size() as usize;
    let (Some(width), Some(owner)) = (indirect_width_at(opcode, native), frames.get_mut(frame)) else {
        return Ok(false);
    };
    let kind = if argument {
        module.method_arg_kind(owner.method, slot)
    } else {
        module.method_local_kind(owner.method, slot)
    };
    let Some((kind, declared)) = kind.and_then(|kind| Some((kind, declared_slot_width(kind, native)?))) else {
        return Ok(false);
    };
    if displacement.checked_add(width).is_none_or(|end| end > declared) {
        return Err(Trap::Unsupported(opcode));
    }
    let slots = if argument { &mut owner.args } else { &mut owner.locals };
    let stored = stack_stored_bytes(opcode, value.clone()).map(u64::from_le_bytes);
    let merged = if displacement == 0 && width == declared {
        match stored {
            // `stind.i` stores a native int into a native int or pointer variable as it is (ECMA-335 6th
            // ed III.3.62 and I.12.1.1.1): a pointer, which has no bytes here, a null reference, which is
            // the null pointer there, or a native int, which four bytes on a 4-byte target could not give
            // back in the form it was stored in ([`stack_loaded_value_at`]).
            _ if kind == SlotKind::Native && opcode == Opcode::StindI => stored_at_slot_kind(Some(kind), value.clone()),
            // Any other full-width store puts the instruction's bytes there, which the variable holds as
            // its declared kind: `*(long*)&p = 0L` leaves a pointer variable the native int 0.
            Ok(bits) => declared_slot_value(kind, bits, native, opcode)?,
            // A value that is not of the instruction's type is not correct CIL (III.3.62), and is
            // refused, as the same store into `stackalloc` memory is.
            Err(trap) => return Err(trap),
        }
    } else {
        let stored = stored.map_err(|_| Trap::Unsupported(opcode))?;
        let current = slots.get(slot).cloned().unwrap_or_else(|| zero_of_slot_kind(kind));
        let bits = declared_slot_bits(kind, &current).ok_or(Trap::Unsupported(opcode))?;
        let shift = displacement * 8;
        let mask = if width >= 8 { u64::MAX } else { (1u64 << (width * 8)) - 1 };
        declared_slot_value(kind, (bits & !(mask << shift)) | ((stored & mask) << shift), native, opcode)?
    };
    set_slot(slots, slot, merged);
    Ok(true)
}

/// Reads with the `ldind.<type>` `opcode` through a pointer to a local or an argument whose declared
/// kind is a number ([`declared_slot_width`]), at the pointer's displacement inside the variable: the
/// instruction loads a value of its type from the address (ECMA-335 6th ed III.3.42), so it reads that
/// many of the variable's declared bytes, which [`store_into_declared_slot`] and the stores of a local
/// or an argument keep at the variable's declared kind. `((byte*)&negative)[1]` of an `int` holding -1
/// reads 255, and `*(uint*)&d` of a `double` its low four bytes (ECMA-334 2nd ed 25.4, 25.5.3 and
/// 25.5.6; the target's byte order, little-endian here, ECMA-335 6th ed I.12.6.3). A read that would run past the
/// variable is refused. `None` where the variable has no such kind, or holds a value that is not its
/// kind's (a managed pointer in a pointer variable), for [`load_from_slot`]'s read.
fn load_from_declared_slot(frames: &[Frame], module: &Module, location: &Location, opcode: Opcode) -> Result<Option<Value>, Trap> {
    let (frame, slot, argument, displacement) = match *location {
        Location::Local { frame, slot } => (frame, slot, false, 0),
        Location::Arg { frame, slot } => (frame, slot, true, 0),
        Location::LocalBytes { frame, slot, byte_offset } => (frame, slot, false, byte_offset as usize),
        Location::ArgBytes { frame, slot, byte_offset } => (frame, slot, true, byte_offset as usize),
        _ => return Ok(None),
    };
    let native = crate::native_pointer_size() as usize;
    let (Some(width), Some(owner)) = (indirect_width_at(opcode, native), frames.get(frame)) else {
        return Ok(None);
    };
    let kind = if argument {
        module.method_arg_kind(owner.method, slot)
    } else {
        module.method_local_kind(owner.method, slot)
    };
    let Some((kind, declared)) = kind.and_then(|kind| Some((kind, declared_slot_width(kind, native)?))) else {
        return Ok(None);
    };
    let current = if argument { owner.args.get(slot) } else { owner.locals.get(slot) };
    let Some((current, bits)) = current.and_then(|current| Some((current, declared_slot_bits(kind, current)?))) else {
        return Ok(None);
    };
    if displacement.checked_add(width).is_none_or(|end| end > declared) {
        return Err(Trap::Unsupported(opcode));
    }
    // A native int variable read whole as a native int is its own value: `ldind.i` loads the native int
    // at the address (ECMA-335 6th ed III.3.42 and I.12.1.1), whatever form it was stored in
    // ([`load_from_slot_at`]).
    if kind == SlotKind::Native && opcode == Opcode::LdindI && displacement == 0 {
        return Ok(Some(current.clone()));
    }
    stack_loaded_value_at(opcode, (bits >> (displacement * 8)).to_le_bytes(), native).map(Some)
}

/// The bytes pointer arithmetic moved a pointer to an array element by, forward or back: the walk keeps
/// them wrapped in a 32-bit count ([`stack_pointer_arithmetic`]), so a step back of four bytes is
/// 0xFFFFFFFC, and an array spans fewer than 2^31 bytes on every target this runs on.
fn element_displacement(byte_offset: u32) -> i64 {
    i64::from(byte_offset as i32)
}

/// The byte at which a pointer to element `index` of an array, moved `byte_offset` bytes on or back by
/// pointer arithmetic ([`element_displacement`]), points into the array's elements, each `width` bytes
/// wide; `None` before the first element or past the range an index can name.
fn element_byte_position(index: usize, byte_offset: u32, width: usize) -> Option<usize> {
    let start = i64::try_from(index.checked_mul(width)?).ok()?;
    usize::try_from(start.checked_add(element_displacement(byte_offset))?).ok()
}

/// Reads with the `ldind.<type>` `opcode` through a pointer into an array whose elements are packed
/// (of a primitive type): the instruction loads a value of its type from the address (ECMA-335 6th ed
/// III.3.42), and the elements are contiguous (I.8.9.1, a rectangular array's in row-major order) in
/// the target's byte order, little-endian on every target this runs on (I.12.6.3), so it reads that
/// many of the array's bytes from the address, inside one element or across several:
/// `((byte*)q)[1]` of an `int[] { 0x04030201 }` reads 2, and `*(int*)p` of a pinned `byte[] { 1, 2,
/// 3, 4 }` 0x04030201 (ECMA-334 2nd ed 25.4 and 25.6). A read past the array is refused. `None` for any
/// other location, and for `ldobj`, which names no width here. A packed array holds no object
/// reference, so `ldind.ref` there reads the native int a pointer is (ECMA-335 6th ed I.12.1.1.1), as
/// lcsc reads `*pp` of a `byte** pp` where csc uses `ldind.i`.
fn load_from_packed_element(vm: &Vm, location: &Location, opcode: Opcode) -> Result<Option<Value>, Trap> {
    let Location::Element { array, index, byte_offset } = *location else {
        return Ok(None);
    };
    let opcode = if opcode == Opcode::LdindRef { Opcode::LdindI } else { opcode };
    let native = crate::native_pointer_size() as usize;
    let (Some(width), Some((bytes, element))) = (indirect_width_at(opcode, native), vm.heap().packed_array_bytes(array)) else {
        return Ok(None);
    };
    let read = element_byte_position(index, byte_offset, element)
        .and_then(|start| bytes.get(start..start.checked_add(width)?))
        .ok_or(Trap::Unsupported(opcode))?;
    let mut raw = [0u8; 8];
    raw[..width].copy_from_slice(read);
    stack_loaded_value_at(opcode, raw, native).map(Some)
}

/// Stores `value` with the `stind.<type>` `opcode` through a pointer into an array whose elements are
/// packed: the instruction stores a value of its type at the address (ECMA-335 6th ed III.3.62), so
/// its bytes replace that many of the array's bytes from the address and the rest are kept, inside
/// one element or across several: `((byte*)q)[1] = 0xAA` into an `int[] { 0x12345678 }` leaves
/// 0x1234AA78, and `*(int*)p = 0x04030201` into a pinned `byte[4]` leaves 1, 2, 3, 4. A store past the
/// array is refused and changes nothing. Returns whether it stored: any other location, and `stobj`,
/// are left to the whole-value store. `stind.ref` there stores the native int a pointer is, as
/// [`load_from_packed_element`] reads it.
fn store_into_packed_element(vm: &mut Vm, location: &Location, opcode: Opcode, value: &Value) -> Result<bool, Trap> {
    let Location::Element { array, index, byte_offset } = *location else {
        return Ok(false);
    };
    let opcode = if opcode == Opcode::StindRef { Opcode::StindI } else { opcode };
    let native = crate::native_pointer_size() as usize;
    let Some(width) = indirect_width_at(opcode, native) else {
        return Ok(false);
    };
    let Some((bytes, element)) = vm.heap_mut().packed_array_bytes_mut(array) else {
        return Ok(false);
    };
    let stored = stack_stored_bytes(opcode, value.clone())?;
    let written = element_byte_position(index, byte_offset, element)
        .and_then(|start| bytes.get_mut(start..start.checked_add(width)?))
        .ok_or(Trap::Unsupported(opcode))?;
    written.copy_from_slice(&stored[..width]);
    Ok(true)
}

/// The local or argument `location` points into, at any displacement: `(frame, slot, argument,
/// displacement)`.
fn variable_location(location: &Location) -> Option<(usize, usize, bool, usize)> {
    match *location {
        Location::Local { frame, slot } => Some((frame, slot, false, 0)),
        Location::Arg { frame, slot } => Some((frame, slot, true, 0)),
        Location::LocalBytes { frame, slot, byte_offset } => Some((frame, slot, false, byte_offset as usize)),
        Location::ArgBytes { frame, slot, byte_offset } => Some((frame, slot, true, byte_offset as usize)),
        _ => None,
    }
}

/// The declared kind of the variable `location` points into and its width, where the loader recorded a
/// number kind for it ([`declared_slot_width`]).
fn declared_number_variable(frames: &[Frame], module: &Module, location: &Location) -> Option<(SlotKind, usize)> {
    let (frame, slot, argument, _) = variable_location(location)?;
    let owner = frames.get(frame)?;
    let kind = if argument { module.method_arg_kind(owner.method, slot) } else { module.method_local_kind(owner.method, slot) }?;
    Some((kind, declared_slot_width(kind, crate::native_pointer_size() as usize)?))
}

/// A pointer into a local or an argument whose declared kind is a struct of a type the loader recorded
/// ([`Module::method_local_struct_type`]): the variable, its type, the byte the pointer reaches inside
/// it, and the storage of what the pointer points at there, where that is the variable itself or one of
/// its fields (`None` for a byte pointer arithmetic reached).
#[derive(Clone, Copy)]
struct StructPointer {
    frame: usize,
    slot: usize,
    argument: bool,
    type_id: TypeId,
    displacement: usize,
    at: Option<crate::module::FieldStorage>,
}

/// The struct variable `location` points into ([`StructPointer`]), `method_of` naming the method each
/// frame runs: the variable's own address, a byte pointer arithmetic moved it to, or the address of a
/// field of it, or of a field of a struct field of it, at the byte the field starts at by the layout
/// of the struct that declares it (ECMA-335 6th ed II.10.1.2). `None` for any other location.
fn struct_pointer(module: &Module, method_of: &impl Fn(usize) -> Option<MethodId>, location: &Location) -> Option<StructPointer> {
    use crate::module::FieldStorage;
    if let Location::Nested { base, slot } = location {
        let outer = struct_pointer(module, method_of, base)?;
        let Some(FieldStorage::ValueType(owner)) = outer.at else {
            return None;
        };
        let slot = *slot as usize;
        let layout = module.type_field_offsets(owner).filter(|layout| slot < layout.slot_count())?;
        let offset = usize::try_from(layout.offset_of(slot)?).ok()?;
        let at = module.type_field_storage(owner, slot)?;
        return Some(StructPointer { displacement: outer.displacement.checked_add(offset)?, at: Some(at), ..outer });
    }
    let (frame, slot, argument, displacement) = variable_location(location)?;
    let method = method_of(frame)?;
    let type_id = if argument { module.method_arg_struct_type(method, slot) } else { module.method_local_struct_type(method, slot) }?;
    if holds_pointer_width(module, type_id, 0) {
        return None;
    }
    Some(StructPointer {
        frame,
        slot,
        argument,
        type_id,
        displacement,
        at: (displacement == 0).then_some(FieldStorage::ValueType(type_id)),
    })
}

/// The method each of `frames` runs, by its index, for [`struct_pointer`].
fn method_of_frame(frames: &[Frame]) -> impl Fn(usize) -> Option<MethodId> + '_ {
    move |index| frames.get(index).map(|frame| frame.method)
}

/// Whether every field of the value type `id`, and of each struct field in it, is a fixed-width number
/// held as bytes here, so its bytes, laid out by its fields (ECMA-335 6th ed II.10.1.2), are its whole
/// value. A field that is a reference is not; nor is a native int or a pointer, whose width, and so the
/// offset of every field after it, is the loading host's (a 64-bit host bakes 8 where a 32-bit board's
/// .NET lays out 4, I.12.1.1). `depth` bounds recursive metadata.
fn has_byte_layout(module: &Module, id: TypeId, depth: usize) -> bool {
    use crate::module::FieldStorage;
    if depth >= MAX_STACK_FIELD_DEPTH {
        return false;
    }
    let Some(layout) = module.type_field_offsets(id) else {
        return false;
    };
    (0..layout.slot_count()).all(|slot| match module.type_field_storage(id, slot) {
        Some(FieldStorage::Primitive(kind)) => kind.packable(),
        Some(FieldStorage::ValueType(inner)) => has_byte_layout(module, inner, depth + 1),
        _ => false,
    })
}

/// Whether the value type `id`, or a struct field in it, has a field as wide as a pointer: a native int,
/// a pointer or a reference (ECMA-335 6th ed I.12.1.1), whose width the loading host's layout took.
/// `true` past `depth`'s bound, where nothing is known.
fn holds_pointer_width(module: &Module, id: TypeId, depth: usize) -> bool {
    use crate::module::FieldStorage;
    if depth >= MAX_STACK_FIELD_DEPTH {
        return true;
    }
    let slots = module.type_field_offsets(id).map_or(0, |layout| layout.slot_count());
    (0..slots).any(|slot| match module.type_field_storage(id, slot) {
        Some(FieldStorage::Native | FieldStorage::Reference) => true,
        Some(FieldStorage::ValueType(inner)) => holds_pointer_width(module, inner, depth + 1),
        _ => false,
    })
}

/// The bytes of the struct variable `pointer` points into, laid out by its type's fields (ECMA-335 6th
/// ed II.10.1.2) from the value it holds, where every field is a number ([`has_byte_layout`]); a struct
/// local no store has reached holds the int32 0 a frame starts it at, and is zero bytes, as a local is
/// with the localsinit flag (II.13.2). `None` for a type with a field that is not a number, and for a
/// value holding one that has no bytes here (a field that points at a variable).
fn struct_variable_bytes(frames: &[Frame], module: &Module, pointer: &StructPointer) -> Option<Vec<u8>> {
    if !has_byte_layout(module, pointer.type_id, 0) {
        return None;
    }
    let mut bytes = alloc::vec![0u8; module.type_field_offsets(pointer.type_id)?.end() as usize];
    let owner = frames.get(pointer.frame)?;
    match if pointer.argument { owner.args.get(pointer.slot) } else { owner.locals.get(pointer.slot) }? {
        Value::Int32(0) => {}
        value => encode_stack_field(module, crate::module::FieldStorage::ValueType(pointer.type_id), &mut bytes, value, 0).ok()?,
    }
    Some(bytes)
}

/// Leaves the struct variable `pointer` points into holding the struct `bytes` make, decoded by its
/// type's field layout (ECMA-335 6th ed II.10.1.2). Bytes that make no value of the type are refused as
/// `opcode`.
fn set_struct_variable_bytes(frames: &mut [Frame], module: &Module, pointer: &StructPointer, bytes: &[u8], opcode: Opcode) -> Result<(), Trap> {
    let storage = crate::module::FieldStorage::ValueType(pointer.type_id);
    let value = decode_stack_field(module, storage, bytes, 0).map_err(|_| Trap::Unsupported(opcode))?;
    let mut kept = alloc::vec![0u8; bytes.len()];
    encode_stack_field(module, storage, &mut kept, &value, 0).map_err(|_| Trap::Unsupported(opcode))?;
    if kept != bytes {
        return Err(Trap::Unsupported(opcode));
    }
    let owner = frames.get_mut(pointer.frame).ok_or(Trap::Unsupported(opcode))?;
    set_slot(if pointer.argument { &mut owner.args } else { &mut owner.locals }, pointer.slot, value);
    Ok(())
}

/// Reads with the `ldind.<type>` `opcode` through a pointer into a struct variable whose fields are all
/// numbers ([`struct_variable_bytes`]): the instruction loads a value of its type from the address
/// (ECMA-335 6th ed III.3.42), so it reads that many of the struct's bytes from the pointer on, within
/// one field or across several: `*(byte*)&s` of a `struct S { int X; }` holding 0x01020304 reads 4, and
/// `*(int*)&s.a` of a `struct S { byte a, b, c, d; }` holding 1, 2, 3 and 4 reads 0x04030201 (ECMA-334
/// 2nd ed 25.4 and 25.5.6; the target's byte order, little-endian here, ECMA-335 6th ed I.12.6.3). A
/// read past the struct is refused. Such a struct holds no object reference, so `ldind.ref` there reads
/// the native int a pointer is (I.12.1.1.1), as lcsc reads `*pp` of a `byte** pp`. `None` for any other
/// location, for a struct holding a value with no bytes here, and for `ldobj`, which moves a value whole.
fn load_from_struct_variable(frames: &[Frame], module: &Module, location: &Location, opcode: Opcode) -> Result<Option<Value>, Trap> {
    let opcode = if opcode == Opcode::LdindRef { Opcode::LdindI } else { opcode };
    let native = crate::native_pointer_size() as usize;
    let Some(width) = indirect_width_at(opcode, native) else {
        return Ok(None);
    };
    let Some(pointer) = struct_pointer(module, &method_of_frame(frames), location) else {
        return Ok(None);
    };
    let Some(bytes) = struct_variable_bytes(frames, module, &pointer) else {
        return Ok(None);
    };
    let read = bytes
        .get(pointer.displacement..pointer.displacement.checked_add(width).ok_or(Trap::Unsupported(opcode))?)
        .ok_or(Trap::Unsupported(opcode))?;
    let mut raw = [0u8; 8];
    raw[..width].copy_from_slice(read);
    stack_loaded_value_at(opcode, raw, native).map(Some)
}

/// Stores `value` with the `stind.<type>` `opcode` through a pointer into a struct variable whose fields
/// are all numbers ([`struct_variable_bytes`]): the instruction stores a value of its type at the address
/// (ECMA-335 6th ed III.3.62), so its bytes replace that many of the struct's bytes from the pointer on
/// and the rest are kept, and the variable holds the struct its bytes then make. A store past the struct
/// is refused and changes nothing. Returns whether it stored: any other location, a struct holding a
/// value with no bytes here, a value with none (a pointer to a variable), and `stobj` are left to the
/// whole-value store. `stind.ref` there stores the native int a pointer is.
fn store_into_struct_variable(frames: &mut [Frame], module: &Module, location: &Location, opcode: Opcode, value: &Value) -> Result<bool, Trap> {
    let opcode = if opcode == Opcode::StindRef { Opcode::StindI } else { opcode };
    let Some(width) = indirect_width_at(opcode, crate::native_pointer_size() as usize) else {
        return Ok(false);
    };
    let Some(pointer) = struct_pointer(module, &method_of_frame(frames), location) else {
        return Ok(false);
    };
    let (Some(mut bytes), Ok(stored)) = (struct_variable_bytes(frames, module, &pointer), stack_stored_bytes(opcode, value.clone())) else {
        return Ok(false);
    };
    let end = pointer.displacement.checked_add(width).ok_or(Trap::Unsupported(opcode))?;
    bytes.get_mut(pointer.displacement..end).ok_or(Trap::Unsupported(opcode))?.copy_from_slice(&stored[..width]);
    set_struct_variable_bytes(frames, module, &pointer, &bytes, opcode)?;
    Ok(true)
}

/// What an [`ObjectPointer`] points into.
#[derive(Clone, Copy)]
enum Holder {
    /// A class instance's fields, numbered by the instance's type's layout.
    Instance { object: ObjectRef, owner: TypeId },
    /// A boxed struct's fields, numbered by the boxed type's layout.
    Boxed { object: ObjectRef, owner: TypeId },
    /// A static field's value, of the storage the loader recorded for it.
    Static { slot: usize, storage: crate::module::FieldStorage },
    /// The elements of an array of structs whose fields are all numbers ([`has_byte_layout`]): the
    /// element type and the bytes its layout gives each element. The pointer's region is the array's.
    Element { array: ObjectRef, owner: TypeId, stride: usize },
}

/// A pointer to a field of a heap object, a class instance's or a boxed struct's, to a static field or a
/// field of a struct static, or into an array of structs: what it points into, the byte the field starts
/// at there, the field's storage, and the bytes around it whose layout is the value's own
/// ([`object_pointer`]).
#[derive(Clone, Copy)]
struct ObjectPointer {
    holder: Holder,
    displacement: usize,
    at: crate::module::FieldStorage,
    region: Option<(usize, usize)>,
}

impl ObjectPointer {
    /// Whether the pointer is into an array of structs at a byte inside an element rather than at an
    /// element's start: a value of the element's own type there is read and written through the
    /// array's bytes, as no element starts at that byte.
    fn inside_an_element(&self) -> bool {
        matches!(self.holder, Holder::Element { stride, .. } if self.displacement % stride != 0)
    }

    /// The bytes a typed access through the pointer may reach: the largest value around it whose fields
    /// are all numbers ([`has_byte_layout`]), else the field it points at.
    fn bound(&self, module: &Module) -> Option<(usize, usize)> {
        match self.region {
            Some(region) => Some(region),
            None => Some((self.displacement, self.displacement.checked_add(field_width(module, self.at)?)?)),
        }
    }
}

/// The bytes from `start` that a field of `storage` there takes, where they are a number or a struct
/// whose fields are all numbers ([`has_byte_layout`]): a region whose layout is the value's own.
fn number_region(module: &Module, storage: crate::module::FieldStorage, start: usize) -> Option<(usize, usize)> {
    use crate::module::FieldStorage;
    match storage {
        FieldStorage::ValueType(id) if !has_byte_layout(module, id, 0) => None,
        FieldStorage::Reference => None,
        storage => Some((start, start.checked_add(field_width(module, storage)?)?)),
    }
}

/// The object or static field `location` points at ([`ObjectPointer`]): an instance field's address, a
/// boxed struct's own address, a static field's address (where the loader recorded its storage), or the
/// address of a field of a struct field of any of them, at the byte the field starts at by the layout of
/// the struct that declares it (ECMA-335 6th ed II.10.1.2). A struct whose fields are all numbers is laid
/// out in sequence and its bytes are its own; a class's fields, and those of a struct holding a
/// reference, are laid out at the runtime's choice (auto layout, II.10.1.2), so the bytes beyond the
/// field or the number struct the pointer is in are not this value's to name, and
/// [`ObjectPointer::bound`] stops there; a static is its own allocation. A pointer into an array of
/// structs whose fields are all numbers names any of the array's bytes: the elements lie one after
/// another (I.8.9.1), each at the stride its layout gives, so a typed access may start inside one
/// element and reach into the next. `None` for any other location, for an array whose elements are
/// packed numbers or hold a reference, and for an object whose layout the loader did not record for
/// the fields it holds.
fn object_pointer(module: &Module, vm: &Vm, location: &Location) -> Option<ObjectPointer> {
    use crate::module::FieldStorage;
    match location {
        Location::Field { object, slot } => {
            let Some(crate::object::Object::Instance { type_id, fields }) = vm.heap().get(*object) else {
                return None;
            };
            let slot = *slot as usize;
            let layout = module.type_field_offsets(*type_id).filter(|layout| layout.slot_count() == fields.len() && slot < fields.len())?;
            let displacement = usize::try_from(layout.offset_of(slot)?).ok()?;
            let at = module.type_field_storage(*type_id, slot)?;
            let holder = Holder::Instance { object: *object, owner: *type_id };
            Some(ObjectPointer { holder, displacement, at, region: number_region(module, at, displacement) })
        }
        Location::Boxed { object } => {
            let Some(crate::object::Object::Boxed { type_token, value: Value::Struct(_) }) = vm.heap().get(*object) else {
                return None;
            };
            let owner = module.type_id_by_handle(*type_token)?;
            let at = FieldStorage::ValueType(owner);
            Some(ObjectPointer { holder: Holder::Boxed { object: *object, owner }, displacement: 0, at, region: number_region(module, at, 0) })
        }
        Location::Static { slot } => {
            let storage = module.static_storage(*slot)?;
            let holder = Holder::Static { slot: *slot, storage };
            Some(ObjectPointer { holder, displacement: 0, at: storage, region: number_region(module, storage, 0) })
        }
        Location::StaticBytes { slot, byte_offset } => {
            let storage = module.static_storage(*slot)?;
            let region = number_region(module, storage, 0)?;
            let displacement = usize::try_from(element_displacement(*byte_offset)).ok()?;
            let holder = Holder::Static { slot: *slot, storage };
            Some(ObjectPointer { holder, displacement, at: storage, region: Some(region) })
        }
        Location::Nested { base, slot } => {
            let outer = object_pointer(module, vm, base)?;
            let FieldStorage::ValueType(inner) = outer.at else {
                return None;
            };
            let slot = *slot as usize;
            let layout = module.type_field_offsets(inner).filter(|layout| slot < layout.slot_count())?;
            let displacement = outer.displacement.checked_add(usize::try_from(layout.offset_of(slot)?).ok()?)?;
            let at = module.type_field_storage(inner, slot)?;
            Some(ObjectPointer { displacement, at, region: outer.region.or_else(|| number_region(module, at, displacement)), ..outer })
        }
        Location::Element { array, index, byte_offset } => {
            let heap = vm.heap();
            if heap.packed_array_bytes(*array).is_some() {
                return None;
            }
            let owner = module.type_id_by_handle(heap.array_element_type(*array)?)?;
            let stride = heap.array_declared_element_width(*array)?;
            let at = FieldStorage::ValueType(owner);
            if !module.type_is_value_type(owner) || !has_byte_layout(module, owner, 0) || field_width(module, at)? != stride {
                return None;
            }
            let length = heap.array_len(*array)?;
            let displacement = element_byte_position(*index, *byte_offset, stride)?;
            let holder = Holder::Element { array: *array, owner, stride };
            Some(ObjectPointer { holder, displacement, at, region: Some((0, length.checked_mul(stride)?)) })
        }
        _ => None,
    }
}

/// The bytes a field of `storage` takes in its type's layout: a primitive's width, a native int's, an
/// object reference's (as wide as a pointer, ECMA-335 6th ed I.12.1.1), and a struct's size.
fn field_width(module: &Module, storage: crate::module::FieldStorage) -> Option<usize> {
    use crate::module::FieldStorage;
    match storage {
        FieldStorage::Primitive(kind) => Some(kind.byte_width()),
        FieldStorage::Native | FieldStorage::Reference => Some(crate::native_pointer_size() as usize),
        FieldStorage::ValueType(id) => usize::try_from(module.type_field_offsets(id)?.end()).ok(),
    }
}

/// The fields of a value of type `owner` that the bytes from `start` to `end` reach, as its layout places
/// them (ECMA-335 6th ed II.10.1.2): each one's slot, storage and byte range, where the record covers
/// `count` fields and the range lies inside the value; `None` otherwise.
fn fields_in_range(module: &Module, owner: TypeId, count: usize, start: usize, end: usize) -> Option<Vec<(usize, crate::module::FieldStorage, usize, usize)>> {
    let layout = module.type_field_offsets(owner).filter(|layout| layout.slot_count() == count)?;
    if start > end || end > usize::try_from(layout.end()).ok()? {
        return None;
    }
    let mut reached = Vec::new();
    for slot in 0..count {
        let storage = module.type_field_storage(owner, slot)?;
        let at = usize::try_from(layout.offset_of(slot)?).ok()?;
        let field_end = at.checked_add(field_width(module, storage)?)?;
        if at < end && start < field_end {
            reached.push((slot, storage, at, field_end));
        }
    }
    Some(reached)
}

/// The bytes from `start` to `start + len` of a value of type `owner` whose fields hold `fields`, as its
/// layout places them (ECMA-335 6th ed II.10.1.2); a byte no field covers, padding, reads as zero. Only
/// the fields the range reaches are read, so a reference elsewhere in the value is no bar. `None` where
/// the range reaches a field with no bytes here (a reference, or a field that points at a variable) or
/// runs past the value. `depth` bounds recursive metadata.
fn fields_range_bytes(module: &Module, owner: TypeId, fields: &[Value], start: usize, len: usize, depth: usize) -> Option<Vec<u8>> {
    use crate::module::FieldStorage;
    if depth >= MAX_STACK_FIELD_DEPTH {
        return None;
    }
    let end = start.checked_add(len)?;
    let mut out = alloc::vec![0u8; len];
    for (slot, storage, at, field_end) in fields_in_range(module, owner, fields.len(), start, end)? {
        let (low, high) = (start.max(at), end.min(field_end));
        let value = fields.get(slot)?;
        let bytes = match (storage, value) {
            (FieldStorage::Reference, _) => return None,
            (FieldStorage::ValueType(inner), Value::Struct(inner_fields)) => fields_range_bytes(module, inner, inner_fields, low - at, high - low, depth + 1)?,
            (storage, value) => {
                let mut whole = alloc::vec![0u8; field_end - at];
                encode_stack_field(module, storage, &mut whole, value, 0).ok()?;
                whole[low - at..high - at].to_vec()
            }
        };
        out[low - start..high - start].copy_from_slice(&bytes);
    }
    Some(out)
}

/// Writes `bytes` from `start` into a value of type `owner` whose fields hold `fields`: each field the
/// bytes reach takes the value its bytes then make, decoded by its storage (ECMA-335 6th ed II.10.1.2),
/// and the fields they do not reach are left as they are. Returns the slots it changed; `None`, and
/// nothing written, where the bytes reach a field with no bytes here or run past the value.
fn write_fields_range(module: &Module, owner: TypeId, fields: &mut [Value], start: usize, bytes: &[u8], depth: usize) -> Option<Vec<usize>> {
    use crate::module::FieldStorage;
    if depth >= MAX_STACK_FIELD_DEPTH {
        return None;
    }
    let end = start.checked_add(bytes.len())?;
    let mut updates = Vec::new();
    for (slot, storage, at, field_end) in fields_in_range(module, owner, fields.len(), start, end)? {
        let (low, high) = (start.max(at), end.min(field_end));
        let written = &bytes[low - start..high - start];
        let value = match (storage, fields.get(slot)?) {
            (FieldStorage::Reference, _) => return None,
            (FieldStorage::ValueType(inner), Value::Struct(inner_fields)) => {
                let mut inner_fields = inner_fields.to_vec();
                write_fields_range(module, inner, &mut inner_fields, low - at, written, depth + 1)?;
                Value::Struct(inner_fields.into_boxed_slice())
            }
            (storage, value) => {
                let mut whole = alloc::vec![0u8; field_end - at];
                encode_stack_field(module, storage, &mut whole, value, 0).ok()?;
                whole[low - at..high - at].copy_from_slice(written);
                decode_stack_field(module, storage, &whole, 0).ok()?
            }
        };
        updates.push((slot, value));
    }
    let changed = updates.iter().map(|&(slot, _)| slot).collect();
    for (slot, value) in updates {
        fields[slot] = value;
    }
    Some(changed)
}

/// The fields of the object `object` holds: an instance's, or the boxed struct's.
fn object_fields(vm: &Vm, object: ObjectRef, boxed: bool) -> Option<&[Value]> {
    match vm.heap().get(object)? {
        crate::object::Object::Instance { fields, .. } if !boxed => Some(fields),
        crate::object::Object::Boxed { value: Value::Struct(fields), .. } if boxed => Some(fields),
        _ => None,
    }
}

/// The bytes from `start` to `start + len` of `value`, a value of `storage`: a struct's by its layout
/// ([`fields_range_bytes`]), any other number's encoded at its width. `None` for a reference, a value
/// with no bytes here, and a range past the value.
fn value_range_bytes(module: &Module, storage: crate::module::FieldStorage, value: &Value, start: usize, len: usize) -> Option<Vec<u8>> {
    use crate::module::FieldStorage;
    match (storage, value) {
        (FieldStorage::Reference, _) => None,
        (FieldStorage::ValueType(owner), Value::Struct(fields)) => fields_range_bytes(module, owner, fields, start, len, 0),
        (storage, value) => {
            let mut whole = alloc::vec![0u8; field_width(module, storage)?];
            encode_stack_field(module, storage, &mut whole, value, 0).ok()?;
            whole.get(start..start.checked_add(len)?).map(<[u8]>::to_vec)
        }
    }
}

/// `value`, a value of `storage`, with `bytes` written from `start` ([`value_range_bytes`]'s inverse):
/// a struct's fields the bytes reach decoded again ([`write_fields_range`]), any other number decoded
/// from its merged bytes. `None` where the bytes reach a reference or run past the value.
fn write_value_range(module: &Module, storage: crate::module::FieldStorage, value: &Value, start: usize, bytes: &[u8]) -> Option<Value> {
    use crate::module::FieldStorage;
    match (storage, value) {
        (FieldStorage::Reference, _) => None,
        (FieldStorage::ValueType(owner), Value::Struct(fields)) => {
            let mut fields = fields.to_vec();
            write_fields_range(module, owner, &mut fields, start, bytes, 0)?;
            Some(Value::Struct(fields.into_boxed_slice()))
        }
        (storage, value) => {
            let mut whole = alloc::vec![0u8; field_width(module, storage)?];
            encode_stack_field(module, storage, &mut whole, value, 0).ok()?;
            whole.get_mut(start..start.checked_add(bytes.len())?)?.copy_from_slice(bytes);
            decode_stack_field(module, storage, &whole, 0).ok()
        }
    }
}

/// Whether `len` bytes from the pointer stay inside the bytes it may reach ([`ObjectPointer::bound`]).
fn object_reach(module: &Module, pointer: &ObjectPointer, len: usize) -> bool {
    pointer.bound(module).is_some_and(|(_, high)| pointer.displacement.checked_add(len).is_some_and(|end| end <= high))
}

/// `len` bytes of the object `pointer` points into, from the pointer on ([`fields_range_bytes`]), where
/// they stay inside the bytes the pointer may reach ([`object_reach`]).
fn object_bytes(module: &Module, vm: &Vm, pointer: &ObjectPointer, len: usize) -> Option<Vec<u8>> {
    if !object_reach(module, pointer, len) {
        return None;
    }
    match pointer.holder {
        Holder::Instance { object, owner } => fields_range_bytes(module, owner, object_fields(vm, object, false)?, pointer.displacement, len, 0),
        Holder::Boxed { object, owner } => fields_range_bytes(module, owner, object_fields(vm, object, true)?, pointer.displacement, len, 0),
        Holder::Static { slot, storage } => value_range_bytes(module, storage, &vm.static_field(slot)?, pointer.displacement, len),
        Holder::Element { array, owner, stride } => {
            let mut bytes = Vec::with_capacity(len);
            for (index, within, chunk) in element_chunks(stride, pointer.displacement, len)? {
                let Value::Struct(fields) = vm.heap().array_get(array, index)? else {
                    return None;
                };
                bytes.extend(fields_range_bytes(module, owner, &fields, within, chunk, 0)?);
            }
            Some(bytes)
        }
    }
}

/// The elements the `len` bytes from the byte `start` of an array cover, whose elements take `stride`
/// bytes each: for each, its index, the byte the range starts at in it, and how many of its bytes the
/// range takes.
fn element_chunks(stride: usize, start: usize, len: usize) -> Option<Vec<(usize, usize, usize)>> {
    let end = start.checked_add(len)?;
    let mut chunks = Vec::new();
    let mut at = start;
    while at < end {
        let (index, within) = (at.checked_div(stride)?, at % stride);
        let chunk = (stride - within).min(end - at);
        chunks.push((index, within, chunk));
        at += chunk;
    }
    Some(chunks)
}

/// Writes `bytes` into the object `pointer` points into, from the pointer on ([`write_fields_range`]),
/// where they stay inside the bytes the pointer may reach ([`object_reach`]); `None`, and nothing
/// written, where they do not, or reach a field with no bytes here.
fn write_object_bytes(module: &Module, vm: &mut Vm, pointer: &ObjectPointer, bytes: &[u8]) -> Option<()> {
    if !object_reach(module, pointer, bytes.len()) {
        return None;
    }
    let keeps = |fields: &[Value], owner: TypeId, start: usize, written: &[u8]| {
        fields_range_bytes(module, owner, fields, start, written.len(), 0).as_deref() == Some(written)
    };
    match pointer.holder {
        Holder::Instance { object, owner } => {
            let mut fields = object_fields(vm, object, false)?.to_vec();
            let slots = write_fields_range(module, owner, &mut fields, pointer.displacement, bytes, 0)?;
            keeps(&fields, owner, pointer.displacement, bytes).then_some(())?;
            for slot in slots {
                let value = fields.get(slot)?.clone();
                vm.heap_mut().set_instance_field(object, u32::try_from(slot).ok()?, value).then_some(())?;
            }
            Some(())
        }
        Holder::Boxed { object, owner } => {
            let mut fields = object_fields(vm, object, true)?.to_vec();
            write_fields_range(module, owner, &mut fields, pointer.displacement, bytes, 0)?;
            keeps(&fields, owner, pointer.displacement, bytes).then_some(())?;
            vm.heap_mut().set_boxed_value(object, Value::Struct(fields.into_boxed_slice())).then_some(())
        }
        Holder::Static { slot, storage } => {
            let value = write_value_range(module, storage, &vm.static_field(slot)?, pointer.displacement, bytes)?;
            (value_range_bytes(module, storage, &value, pointer.displacement, bytes.len()).as_deref() == Some(bytes)).then_some(())?;
            vm.set_static_field(slot, value);
            Some(())
        }
        Holder::Element { array, owner, stride } => {
            let mut written = Vec::new();
            let mut from = 0;
            for (index, within, chunk) in element_chunks(stride, pointer.displacement, bytes.len())? {
                let Value::Struct(fields) = vm.heap().array_get(array, index)? else {
                    return None;
                };
                let mut fields = fields.to_vec();
                let part = bytes.get(from..from + chunk)?;
                write_fields_range(module, owner, &mut fields, within, part, 0)?;
                keeps(&fields, owner, within, part).then_some(())?;
                written.push((index, Value::Struct(fields.into_boxed_slice())));
                from += chunk;
            }
            for (index, value) in written {
                vm.heap_mut().array_set(array, index, value).then_some(())?;
            }
            Some(())
        }
    }
}

/// Reads with the `ldind.<type>` `opcode` through a pointer to a field of a heap object ([`object_pointer`]):
/// the instruction loads a value of its type from the address (ECMA-335 6th ed III.3.42), so it reads that
/// many bytes from the field on, as its storage and the layout of the number struct it is in place them
/// (II.10.1.2): `*(int*)&o.F` of a `float` field holding 1.0f reads 0x3F800000, and the address of a boxed
/// struct `Halves` holding the shorts 1 and 2 reads 0x00020001 as an `int` (ECMA-334 2nd ed 25.4). A
/// pointer into an array of such structs reads the array's bytes, inside one element or across two:
/// `*(long*)((byte*)p + 4)` of a pinned `P2[]` holds the first element's second `int` and the second's
/// first (I.8.9.1). A read past the bytes the pointer may reach ([`ObjectPointer::bound`]), as past a
/// class's field into the fields the runtime laid out after it, or past the array, is refused. `ldind.ref` over numbers reads the native int a
/// pointer is (I.12.1.1.1). `None` for any other location and for `ldobj`, and where the bytes reach a
/// reference or a field with no bytes here, for the read that answers from the field's value.
fn load_from_object_fields(vm: &Vm, module: &Module, location: &Location, requested: Opcode) -> Result<Option<Value>, Trap> {
    let opcode = if requested == Opcode::LdindRef { Opcode::LdindI } else { requested };
    let native = crate::native_pointer_size() as usize;
    let Some(width) = indirect_width_at(opcode, native) else {
        return Ok(None);
    };
    let Some(pointer) = object_pointer(module, vm, location) else {
        return Ok(None);
    };
    if !object_reach(module, &pointer, width) {
        return Err(Trap::Unsupported(requested));
    }
    let Some(bytes) = object_bytes(module, vm, &pointer, width) else {
        return Ok(None);
    };
    let mut raw = [0u8; 8];
    raw[..width].copy_from_slice(&bytes);
    stack_loaded_value_at(opcode, raw, native).map(Some)
}

/// Stores `value` with the `stind.<type>` `opcode` through a pointer to a field of a heap object
/// ([`object_pointer`]): the instruction stores a value of its type at the address (ECMA-335 6th ed
/// III.3.62), so its bytes replace that many from the field on, and each field they reach holds the
/// value its bytes then make, in each element they reach of an array of structs. A store past the bytes
/// the pointer may reach ([`ObjectPointer::bound`]) is refused and changes nothing. Returns whether it stored: any other location, `stobj`, a value with no
/// bytes here (a pointer to a variable), and bytes that reach a reference are left to the whole-value
/// store. `stind.ref` over numbers stores the native int a pointer is.
fn store_into_object_fields(vm: &mut Vm, module: &Module, location: &Location, requested: Opcode, value: &Value) -> Result<bool, Trap> {
    let opcode = if requested == Opcode::StindRef { Opcode::StindI } else { requested };
    let Some(width) = indirect_width_at(opcode, crate::native_pointer_size() as usize) else {
        return Ok(false);
    };
    let Some(pointer) = object_pointer(module, vm, location) else {
        return Ok(false);
    };
    if !object_reach(module, &pointer, width) {
        return Err(Trap::Unsupported(requested));
    }
    let Ok(stored) = stack_stored_bytes(opcode, value.clone()) else {
        return Ok(false);
    };
    match write_object_bytes(module, vm, &pointer, &stored[..width]) {
        Some(()) => Ok(true),
        None if pointer.region.is_some() => Err(Trap::Unsupported(requested)),
        None => Ok(false),
    }
}

/// `value` with a pointer to a field of a struct variable ([`Location::Nested`] over a [`StructPointer`])
/// given as the byte of the variable the field starts at ([`Location::LocalBytes`] or
/// [`Location::ArgBytes`]), the form pointer arithmetic walks and pointer comparison and difference
/// read (ECMA-335 6th ed III.1.5): `(byte*)&s.b - (byte*)&s` is the field's offset. Any other value as
/// it is.
fn as_variable_bytes(module: Option<&Module>, method_of: &impl Fn(usize) -> Option<MethodId>, value: Value) -> Value {
    let (Some(module), Value::ByRef(location @ Location::Nested { .. })) = (module, &value) else {
        return value;
    };
    let Some(pointer) = struct_pointer(module, method_of, location) else {
        return value;
    };
    let Ok(byte_offset) = u32::try_from(pointer.displacement) else {
        return value;
    };
    Value::ByRef(if pointer.argument {
        Location::ArgBytes { frame: pointer.frame, slot: pointer.slot, byte_offset }
    } else {
        Location::LocalBytes { frame: pointer.frame, slot: pointer.slot, byte_offset }
    })
}

/// The operands of a binary operation or a comparison in the frame at `frame_index`, which runs
/// `method` above `callers`, each pointer to a field of a struct variable given as the byte of the
/// variable it points at ([`as_variable_bytes`]). Only a field's address changes, so integer operands,
/// the hot case, are passed through untouched.
fn variable_bytes_pair(module: Option<&Module>, method: MethodId, frame_index: usize, callers: &[Frame], a: Value, b: Value) -> (Value, Value) {
    let nested = |value: &Value| matches!(value, Value::ByRef(Location::Nested { .. }));
    if !nested(&a) && !nested(&b) {
        return (a, b);
    }
    let method_of = |index: usize| if index == frame_index { Some(method) } else { callers.get(index).map(|frame| frame.method) };
    (as_variable_bytes(module, &method_of, a), as_variable_bytes(module, &method_of, b))
}

/// Whether the storage `location` points into holds bytes that a typed access reads and writes: a
/// local or an argument whose declared kind is a number, or an array whose elements are packed.
fn holds_bytes(frames: &[Frame], vm: &Vm, module: &Module, location: &Location) -> bool {
    match *location {
        Location::Element { array, .. } => vm.heap().packed_array_bytes(array).is_some(),
        Location::StringChar { .. } => true,
        _ => declared_number_variable(frames, module, location).is_some(),
    }
}

/// The storage of a value of the type `token` names in assembly `asm`, as `ldobj` and `stobj` move it:
/// an enum's is its underlying integer's (ECMA-335 6th ed II.14.3), a built-in value type's its
/// primitive or native int, any other value type's its fields by its layout, a reference type's a
/// reference. `None` for a type the module recorded no id for.
fn obj_storage(module: &Module, asm: u8, token: Token) -> Option<crate::module::FieldStorage> {
    Some(storage_of_type(module, module.type_id_of(asm, token)?))
}

/// The storage of a value of the type `id`, as [`obj_storage`] gives a token's.
fn storage_of_type(module: &Module, id: TypeId) -> crate::module::FieldStorage {
    use crate::module::FieldStorage;
    if let Some(kind) = module
        .type_handle_of(id)
        .and_then(|handle| module.enum_underlying_prim_by_handle(handle))
        .and_then(CastPrim::prim_kind)
    {
        return FieldStorage::Primitive(kind);
    }
    match builtin_storage(module, id) {
        Some(storage) => storage,
        None if module.type_is_value_type(id) => FieldStorage::ValueType(id),
        None => FieldStorage::Reference,
    }
}

/// The declared storage of the whole value `location` names, where the loader recorded it: a local's or
/// an argument's (its struct type, or its slot kind), an object's field's, a static's, a boxed value's,
/// an array element's, or a field's of any of those that is a struct, an enum's being its underlying
/// integer's (ECMA-335 6th ed II.14.3). `None` where nothing was recorded, and for a pointer moved into a
/// value, which names no whole one.
fn declared_storage_at(
    method_of: &impl Fn(usize) -> Option<MethodId>, vm: &Vm, module: &Module, location: &Location,
) -> Option<crate::module::FieldStorage> {
    use crate::module::FieldStorage;
    let declared = match location {
        Location::Local { frame, slot } | Location::LocalBytes { frame, slot, byte_offset: 0 } => {
            variable_storage(method_of, module, *frame, *slot, false)?
        }
        Location::Arg { frame, slot } | Location::ArgBytes { frame, slot, byte_offset: 0 } => {
            variable_storage(method_of, module, *frame, *slot, true)?
        }
        Location::Field { object, slot } => match vm.heap().get(*object) {
            Some(crate::object::Object::Instance { type_id, .. }) => module.type_field_storage(*type_id, *slot as usize)?,
            _ => return None,
        },
        Location::Static { slot } | Location::StaticBytes { slot, byte_offset: 0 } => module.static_storage(*slot)?,
        Location::Boxed { object } => storage_of_type(module, module.type_id_by_handle(vm.heap().boxed_type_token(*object)?)?),
        Location::Element { array, index, byte_offset } => {
            array_element_index(vm, *array, *index, *byte_offset)?;
            storage_of_type(module, module.type_id_by_handle(vm.heap().array_element_type(*array)?)?)
        }
        Location::Nested { base, slot } => match declared_storage_at(method_of, vm, module, base)? {
            FieldStorage::ValueType(owner) => module.type_field_storage(owner, *slot as usize)?,
            _ => return None,
        },
        _ => return None,
    };
    Some(match declared {
        FieldStorage::ValueType(id) => storage_of_type(module, id),
        storage => storage,
    })
}

/// The declared storage of the local or argument `slot` of the method frame `frame` runs, by the slot
/// kind the loader recorded and, for a struct, its type.
fn variable_storage(
    method_of: &impl Fn(usize) -> Option<MethodId>, module: &Module, frame: usize, slot: usize, argument: bool,
) -> Option<crate::module::FieldStorage> {
    use crate::module::FieldStorage;
    let method = method_of(frame)?;
    let kind = if argument { module.method_arg_kind(method, slot) } else { module.method_local_kind(method, slot) }?;
    Some(match kind {
        SlotKind::Primitive(prim) => FieldStorage::Primitive(prim),
        SlotKind::Native => FieldStorage::Native,
        SlotKind::Reference => FieldStorage::Reference,
        SlotKind::Struct => FieldStorage::ValueType(if argument {
            module.method_arg_struct_type(method, slot)
        } else {
            module.method_local_struct_type(method, slot)
        }?),
        SlotKind::ByRef | SlotKind::Unknown => return None,
    })
}

/// The storage of the value the `stind.<type>` `store` stores, or, for a `stobj` that moves a value
/// whole, of the type `token` names in assembly `asm`'s tokens.
fn stored_storage(module: &Module, asm: u8, store: Opcode, token: Token) -> Option<crate::module::FieldStorage> {
    use crate::module::FieldStorage;
    Some(match store {
        Opcode::StindI1 => FieldStorage::Primitive(PrimKind::I1),
        Opcode::StindI2 => FieldStorage::Primitive(PrimKind::I2),
        Opcode::StindI4 => FieldStorage::Primitive(PrimKind::I4),
        Opcode::StindI8 => FieldStorage::Primitive(PrimKind::I8),
        Opcode::StindR4 => FieldStorage::Primitive(PrimKind::F4),
        Opcode::StindR8 => FieldStorage::Primitive(PrimKind::F8),
        Opcode::StindI => FieldStorage::Native,
        Opcode::StindRef => FieldStorage::Reference,
        Opcode::Stobj => return obj_storage(module, asm, token),
        _ => return None,
    })
}

/// Whether a value of `stored`'s storage replaces a whole value of `declared`'s: a number of the same
/// width and the same kind, integer or floating; a native int over a native int and a reference over a
/// reference; or a struct over one of its own type.
fn whole_store_fits(declared: crate::module::FieldStorage, stored: crate::module::FieldStorage) -> bool {
    use crate::module::FieldStorage;
    let floating = |kind: PrimKind| matches!(kind, PrimKind::F4 | PrimKind::F8);
    match (declared, stored) {
        (FieldStorage::Primitive(a), FieldStorage::Primitive(b)) => a.byte_width() == b.byte_width() && floating(a) == floating(b),
        (FieldStorage::Native, FieldStorage::Native) | (FieldStorage::Reference, FieldStorage::Reference) => true,
        (FieldStorage::ValueType(a), FieldStorage::ValueType(b)) => a == b,
        _ => false,
    }
}

/// Whether the value `held` is in the representation of `stored`'s storage: an int64 for an int64, a
/// float of its width for a float, a native int for a native int, a reference for a reference, and an
/// int32 for an int32. A narrower integer is held as an int32 too, so an int32 value fits an int32's
/// storage whether or not it came from a narrower type, and fits no narrower storage. A struct, whose
/// type the value does not carry, and nothing at all fit no storage.
fn unrecorded_store_fits(held: Option<&Value>, stored: Option<crate::module::FieldStorage>) -> bool {
    use crate::module::FieldStorage;
    match (held, stored) {
        (Some(Value::Int32(_)), Some(FieldStorage::Primitive(PrimKind::I4)))
        | (Some(Value::Int64(_)), Some(FieldStorage::Primitive(PrimKind::I8)))
        | (Some(Value::NativeInt(_)), Some(FieldStorage::Native))
        | (Some(Value::Object(_) | Value::Null), Some(FieldStorage::Reference)) => true,
        #[cfg(feature = "float")]
        (Some(Value::Single(_)), Some(FieldStorage::Primitive(PrimKind::F4)))
        | (Some(Value::Float(_)), Some(FieldStorage::Primitive(PrimKind::F8))) => true,
        _ => false,
    }
}

/// Whether `value` is a floating value and `declared` a floating storage, of either width.
fn floating_into_floating(declared: crate::module::FieldStorage, value: &Value) -> bool {
    #[cfg(feature = "float")]
    if let crate::module::FieldStorage::Primitive(PrimKind::F4 | PrimKind::F8) = declared {
        return matches!(value, Value::Single(_) | Value::Float(_));
    }
    let _ = (declared, value);
    false
}

/// `value`, an integer `stind` stores whole into a number of `declared` storage of its width, as that
/// storage holds it: a byte or a short with the declared sign, so `stind.i1` of 255 into a `byte` holds
/// 255 (CIL's `stind` has no unsigned form, ECMA-335 6th ed III.3.62). Any other value as it is.
fn as_declared_integer(declared: crate::module::FieldStorage, value: Value) -> Value {
    use crate::module::FieldStorage;
    match (declared, value) {
        (FieldStorage::Primitive(PrimKind::U1), Value::Int32(n)) => Value::Int32(i32::from(n as u8)),
        (FieldStorage::Primitive(PrimKind::I1), Value::Int32(n)) => Value::Int32(i32::from(n as i8)),
        (FieldStorage::Primitive(PrimKind::U2), Value::Int32(n)) => Value::Int32(i32::from(n as u16)),
        (FieldStorage::Primitive(PrimKind::I2), Value::Int32(n)) => Value::Int32(i32::from(n as i16)),
        (_, value) => value,
    }
}

/// The value the whole-value store `stind` and `stobj` come to, when no byte path took them (a
/// variable's declared bytes, a packed array's, a struct variable's, an object's), writes at `location`,
/// or a refusal, as `store`, where it would replace something other than a value of the store's type:
/// the instruction stores a value of its type at the address and changes nothing else (ECMA-335 6th ed
/// III.3.62 and III.4.29). Where the loader recorded the storage the location names
/// ([`declared_storage_at`]), the store's must fit it ([`whole_store_fits`]) and the value takes its
/// sign ([`as_declared_integer`]). Two stores are judged otherwise there, and their values kept as they
/// are: a pointer or a null reference that `stind.ref` stores into a native int or a pointer, which is
/// taken; and a `stobj` of a type the module names no id for, which is judged by its value alone
/// ([`unrecorded_store_fits`], [`floating_into_floating`]), so an int32 value replaces a whole int32
/// though it may have come from a narrower type, and a floating value replaces a floating one of either
/// width. Where it recorded none, the value replaces what the location holds unless that is a struct and
/// the value is not: a store there cannot be told from one into part of a wider value, and a verifiable
/// store, which stores a value of the location's own type, must go on. Any other store into recorded
/// storage that does not fit is refused, and is not made to replace the whole variable, field or element
/// it points into.
fn checked_whole_store(
    frames: &[Frame], vm: &Vm, module: &Module, location: &Location, store: Opcode, stored: Option<crate::module::FieldStorage>, value: Value,
) -> Result<Value, Trap> {
    use crate::module::FieldStorage;
    match (declared_storage_at(&method_of_frame(frames), vm, module, location), stored) {
        (Some(declared), Some(stored)) if whole_store_fits(declared, stored) => Ok(as_declared_integer(declared, value)),
        (Some(FieldStorage::Native), Some(FieldStorage::Reference))
            if matches!(value, Value::Null | Value::NativeInt(_) | Value::ByRef(_)) => Ok(value),
        (Some(declared), None) if unrecorded_store_fits(Some(&value), Some(declared)) || floating_into_floating(declared, &value) => Ok(value),
        (None, _) if matches!(value, Value::Struct(_)) || !matches!(read_location_value(frames, vm, location.clone()), Some(Value::Struct(_))) => Ok(value),
        _ => Err(Trap::Unsupported(store)),
    }
}

/// The storage of the built-in value type `id` names, a primitive or a native int, by its full name
/// (ECMA-335 6th ed I.8.2.2); `None` for any other type.
fn builtin_storage(module: &Module, id: TypeId) -> Option<crate::module::FieldStorage> {
    use crate::module::FieldStorage;
    Some(match module.type_full_name(id)? {
        "System.SByte" => FieldStorage::Primitive(PrimKind::I1),
        "System.Byte" | "System.Boolean" => FieldStorage::Primitive(PrimKind::U1),
        "System.Int16" => FieldStorage::Primitive(PrimKind::I2),
        "System.UInt16" | "System.Char" => FieldStorage::Primitive(PrimKind::U2),
        "System.Int32" | "System.UInt32" => FieldStorage::Primitive(PrimKind::I4),
        "System.Int64" | "System.UInt64" => FieldStorage::Primitive(PrimKind::I8),
        "System.Single" => FieldStorage::Primitive(PrimKind::F4),
        "System.Double" => FieldStorage::Primitive(PrimKind::F8),
        "System.IntPtr" | "System.UIntPtr" => FieldStorage::Native,
        _ => return None,
    })
}

/// Reads with the `ldind.<type>` (or `ldobj`) `opcode` through a managed pointer to `location`: from the
/// bytes of a variable whose declared kind is a number ([`load_from_declared_slot`]), of a packed array
/// ([`load_from_packed_element`]), of a struct variable ([`load_from_struct_variable`]), of an object's or
/// a static's field or an array of structs ([`load_from_object_fields`]) or of a pinned string ([`load_from_string_chars`]), and
/// anywhere else from the value the slot holds ([`load_from_slot`]).
fn typed_load(frames: &[Frame], vm: &Vm, module: &Module, location: Location, opcode: Opcode) -> Result<Value, Trap> {
    if let Some(value) = load_from_declared_slot(frames, module, &location, opcode)? {
        return Ok(value);
    }
    if let Some(value) = load_from_packed_element(vm, &location, opcode)? {
        return Ok(value);
    }
    if let Some(value) = load_from_struct_variable(frames, module, &location, opcode)? {
        return Ok(value);
    }
    if let Some(value) = load_from_object_fields(vm, module, &location, opcode)? {
        return Ok(value);
    }
    if let Some(value) = load_from_string_chars(vm, &location, opcode)? {
        return Ok(value);
    }
    let slot = load_slot(vm, location, opcode)?;
    let declared = declared_width(vm, &slot);
    let variable = is_variable_address(&slot);
    load_from_slot(read_byref(frames, vm, slot, opcode)?, opcode, declared, variable)
}

/// The receiver of a runtime-provided method of a built-in value type, where it is a managed pointer:
/// read as the `ldind` of that type reads it ([`typed_load`]), so `(*(float*)&bits).ToString()` of a
/// `uint` holding 0x3F800000 formats the float 1 and `chars[1].ToString()` over an `int` the second
/// `char` its bytes make (ECMA-335 6th ed II.13.3: a value type's instance method takes its value
/// through a managed pointer, and III.3.42). No static method of a built-in value type takes a
/// managed pointer first, so a pointer there is the receiver; read at its own type, a receiver whose
/// storage is that type reads as it did.
fn read_builtin_receiver(frames: &[Frame], vm: &Vm, module: &Module, method: MethodId, args: &mut [Value]) -> Result<(), Trap> {
    let Some(Value::ByRef(location)) = args.first() else {
        return Ok(());
    };
    let Some((load, _)) = module.method_type(method).and_then(|id| builtin_storage(module, id)).and_then(stack_field_opcodes) else {
        return Ok(());
    };
    args[0] = typed_load(frames, vm, module, location.clone(), load)?;
    Ok(())
}

/// How `ldobj` or `stobj` (`opcode`) of the type `token` names moves a value through a pointer to
/// `location`, in assembly `asm`'s tokens. Where the pointer's storage holds bytes ([`holds_bytes`]),
/// a primitive or an enum moves as the `ldind` or `stind` of its type, those instructions being
/// `ldobj` and `stobj` of the built-in types (ECMA-335 6th ed III.4.13 and III.4.29), and a struct as
/// the bytes its field layout gives it (the second value, its type, II.10.1.2). In a struct variable
/// ([`struct_pointer`]) the value of the type the pointer points at, the variable's own or a field's,
/// moves whole, as it is held, and a value of another type through the struct's bytes where its fields
/// are all numbers ([`has_byte_layout`]): `*(S*)&h` of a `Halves h` holding the shorts 1 and 2 is the
/// `S` whose `int` is 0x00020001. Anywhere else, and for a type that names no storage, the instruction
/// moves the value whole, as it is held.
fn obj_access(frames: &[Frame], vm: &Vm, module: &Module, asm: u8, location: &Location, opcode: Opcode, token: Token) -> (Opcode, Option<TypeId>) {
    use crate::module::FieldStorage;
    if !matches!(opcode, Opcode::Ldobj | Opcode::Stobj) {
        return (opcode, None);
    }
    let storage = obj_storage(module, asm, token);
    let through_bytes = match struct_pointer(module, &method_of_frame(frames), location) {
        Some(pointer) => storage.is_some() && pointer.at != storage && has_byte_layout(module, pointer.type_id, 0),
        None => match object_pointer(module, vm, location) {
            Some(pointer) => storage.is_some() && (Some(pointer.at) != storage || pointer.inside_an_element()),
            None => holds_bytes(frames, vm, module, location),
        },
    };
    if !through_bytes {
        return (opcode, None);
    }
    match storage {
        Some(FieldStorage::ValueType(id)) => (opcode, Some(id)),
        Some(storage) => match stack_field_opcodes(storage) {
            Some((load, store)) => (if opcode == Opcode::Ldobj { load } else { store }, None),
            None => (opcode, None),
        },
        None => (opcode, None),
    }
}

/// `len` bytes of the storage `location` points into, from the pointer on: a variable's declared bytes
/// ([`declared_slot_bits`]), a struct variable's ([`struct_variable_bytes`]), a packed array's, or an
/// object's, a static's or an array of structs' ([`object_bytes`]), in the target's byte order
/// (ECMA-335 6th ed I.12.6.3). A read past the storage, or of a variable holding
/// a value that has no bytes here, is refused as `opcode`.
fn bytes_at(frames: &[Frame], vm: &Vm, module: &Module, location: &Location, len: usize, opcode: Opcode) -> Result<Vec<u8>, Trap> {
    let refused = || Trap::Unsupported(opcode);
    if let Location::Element { array, index, byte_offset } = *location
        && let Some((bytes, element)) = vm.heap().packed_array_bytes(array)
    {
        let start = element_byte_position(index, byte_offset, element).ok_or_else(refused)?;
        return bytes.get(start..start.checked_add(len).ok_or_else(refused)?).map(<[u8]>::to_vec).ok_or_else(refused);
    }
    if let Some(pointer) = struct_pointer(module, &method_of_frame(frames), location) {
        let bytes = struct_variable_bytes(frames, module, &pointer).ok_or_else(refused)?;
        let end = pointer.displacement.checked_add(len).ok_or_else(refused)?;
        return bytes.get(pointer.displacement..end).map(<[u8]>::to_vec).ok_or_else(refused);
    }
    if let Some(pointer) = object_pointer(module, vm, location) {
        return object_bytes(module, vm, &pointer, len).ok_or_else(refused);
    }
    if let Location::StringChar { string, byte_offset } = *location {
        let units = vm.heap().as_string(string).ok_or(Trap::NullReference)?;
        return (0..len)
            .map(|index| (byte_offset as usize).checked_add(index).and_then(|at| string_byte_at(&units, at)).ok_or_else(refused))
            .collect();
    }
    let (frame, slot, argument, displacement) = variable_location(location).ok_or_else(refused)?;
    let (kind, declared) = declared_number_variable(frames, module, location).ok_or_else(refused)?;
    let owner = frames.get(frame).ok_or_else(refused)?;
    let current = if argument { owner.args.get(slot) } else { owner.locals.get(slot) };
    let bits = current.and_then(|current| declared_slot_bits(kind, current)).ok_or_else(refused)?;
    let end = displacement.checked_add(len).filter(|&end| end <= declared).ok_or_else(refused)?;
    Ok(bits.to_le_bytes()[displacement..end].to_vec())
}

/// Writes `bytes` into the storage `location` points into, from the pointer on, keeping the bytes it
/// does not cover: into a variable's declared bytes, which the variable then holds as its declared
/// kind ([`declared_slot_value`]), into a struct variable's, which it then holds as the struct they
/// make ([`set_struct_variable_bytes`]), into a packed array's, or into an object's, a static's or an
/// array of structs' ([`write_object_bytes`]). A write past the storage, or into a
/// variable holding a value that has no bytes here, is refused as `opcode` and changes nothing.
fn write_bytes_at(frames: &mut [Frame], vm: &mut Vm, module: &Module, location: &Location, bytes: &[u8], opcode: Opcode) -> Result<(), Trap> {
    let refused = || Trap::Unsupported(opcode);
    if let Location::Element { array, index, byte_offset } = *location
        && let Some((storage, element)) = vm.heap_mut().packed_array_bytes_mut(array)
    {
        let start = element_byte_position(index, byte_offset, element).ok_or_else(refused)?;
        let end = start.checked_add(bytes.len()).ok_or_else(refused)?;
        storage.get_mut(start..end).ok_or_else(refused)?.copy_from_slice(bytes);
        return Ok(());
    }
    let pointer = struct_pointer(module, &method_of_frame(frames), location);
    if let Some(pointer) = pointer {
        let mut merged = struct_variable_bytes(frames, module, &pointer).ok_or_else(refused)?;
        let end = pointer.displacement.checked_add(bytes.len()).ok_or_else(refused)?;
        merged.get_mut(pointer.displacement..end).ok_or_else(refused)?.copy_from_slice(bytes);
        return set_struct_variable_bytes(frames, module, &pointer, &merged, opcode);
    }
    if let Some(pointer) = object_pointer(module, vm, location) {
        return write_object_bytes(module, vm, &pointer, bytes).ok_or_else(refused);
    }
    let (frame, slot, argument, displacement) = variable_location(location).ok_or_else(refused)?;
    let (kind, declared) = declared_number_variable(frames, module, location).ok_or_else(refused)?;
    let end = displacement.checked_add(bytes.len()).filter(|&end| end <= declared).ok_or_else(refused)?;
    let owner = frames.get_mut(frame).ok_or_else(refused)?;
    let slots = if argument { &mut owner.args } else { &mut owner.locals };
    let current = slots.get(slot).cloned().unwrap_or_else(|| zero_of_slot_kind(kind));
    let mut merged = declared_slot_bits(kind, &current).ok_or_else(refused)?.to_le_bytes();
    merged[displacement..end].copy_from_slice(bytes);
    let value = declared_slot_value(kind, u64::from_le_bytes(merged), crate::native_pointer_size() as usize, opcode)?;
    set_slot(slots, slot, value);
    Ok(())
}

/// Reads with `ldobj` the struct of type `id` from the bytes of the storage `location` points into
/// ([`bytes_at`]), decoded by its field layout (ECMA-335 6th ed III.4.13 and II.10.1.2): `*(Wrap*)&n`
/// of an `int n = 42` is the `Wrap` whose `int` field holds 42. A struct with a field that is not a
/// number has no bytes to make it from, and is refused.
fn load_struct_from_bytes(frames: &[Frame], vm: &Vm, module: &Module, location: &Location, id: TypeId) -> Result<Value, Trap> {
    let layout = module.type_field_offsets(id).ok_or(Trap::Unsupported(Opcode::Ldobj))?;
    let bytes = bytes_at(frames, vm, module, location, layout.end() as usize, Opcode::Ldobj)?;
    decode_stack_field(module, crate::module::FieldStorage::ValueType(id), &bytes, 0).map_err(|_| Trap::Unsupported(Opcode::Ldobj))
}

/// Stores with `stobj` the struct `value` of type `id` into the bytes of the storage `location` points
/// into, encoded by its field layout (ECMA-335 6th ed III.4.29 and II.10.1.2) and written with
/// [`write_bytes_at`]. A struct with a field that is not a number is refused.
fn store_struct_into_bytes(frames: &mut [Frame], vm: &mut Vm, module: &Module, location: &Location, id: TypeId, value: &Value) -> Result<(), Trap> {
    let layout = module.type_field_offsets(id).ok_or(Trap::Unsupported(Opcode::Stobj))?;
    let mut bytes = alloc::vec![0u8; layout.end() as usize];
    encode_stack_field(module, crate::module::FieldStorage::ValueType(id), &mut bytes, value, 0)
        .map_err(|_| Trap::Unsupported(Opcode::Stobj))?;
    write_bytes_at(frames, vm, module, location, &bytes, Opcode::Stobj)
}

/// The declared kind of the local or argument `location` names at its own address, where the loader
/// recorded it: a whole value stored there through the address, with `stobj`, `stind.ref` or `cpobj`
/// (ECMA-335 6th ed III.4.29, III.3.62 and III.4.4), takes that kind as a `stloc` or a `starg` of it does
/// ([`stored_at_slot_kind`]), so a null reference stored in a pointer variable is the null pointer and
/// a float32 stored in a float64 variable the float64 of its value (I.12.1.1.1 and I.12.1.3).
fn declared_variable_kind(frames: &[Frame], module: &Module, location: &Location) -> Option<SlotKind> {
    let (frame, slot, argument) = match *location {
        Location::Local { frame, slot } | Location::LocalBytes { frame, slot, byte_offset: 0 } => (frame, slot, false),
        Location::Arg { frame, slot } | Location::ArgBytes { frame, slot, byte_offset: 0 } => (frame, slot, true),
        _ => return None,
    };
    let owner = frames.get(frame)?;
    if argument {
        module.method_arg_kind(owner.method, slot)
    } else {
        module.method_local_kind(owner.method, slot)
    }
}

/// `value` as a field or a static of declared `storage` holds it: a floating value at the storage's
/// floating width, as a value stored in a field or a static takes its type's (ECMA-335 6th ed I.12.1.3:
/// statics and fields of classes are of fixed size, and a floating value put there is coerced to it),
/// and a null reference in a native int or pointer field as the native int 0, the null pointer
/// (I.12.1.1.1). Any other value is kept, as is any value where no storage was recorded.
fn stored_at_storage(storage: Option<crate::module::FieldStorage>, value: Value) -> Value {
    use crate::module::FieldStorage;
    match (storage, value) {
        #[cfg(feature = "float")]
        (Some(FieldStorage::Primitive(PrimKind::F8)), Value::Single(f)) => Value::Float(f64::from(f)),
        #[cfg(feature = "float")]
        (Some(FieldStorage::Primitive(PrimKind::F4)), Value::Float(f)) => Value::Single(f as f32),
        (Some(FieldStorage::Native), Value::Null) => Value::NativeInt(0),
        (_, value) => value,
    }
}

/// Whether a value stored in a local or an argument may need the slot's declared kind
/// ([`stored_at_slot_kind`]): a floating value or a null reference.
fn depends_on_slot_kind(value: &Value) -> bool {
    holds_floating(value) || matches!(value, Value::Null)
}

/// Converts the value on top of `frame`'s stack to the declared kind of the local or argument it is
/// about to be stored in ([`stored_at_slot_kind`]); `kind` is asked only for a value that may need it.
fn declare_top(frame: &mut Frame, kind: impl FnOnce(MethodId) -> Option<SlotKind>) {
    let method = frame.method;
    if let Some(top) = frame.stack.last_mut() {
        if depends_on_slot_kind(top) {
            let value = core::mem::replace(top, Value::Null);
            *top = stored_at_slot_kind(kind(method), value);
        }
    }
}

/// A new frame's arguments at their declared kinds ([`stored_at_slot_kind`]) and its locals at their
/// declared kinds' zeros ([`zero_of_slot_kind`]), where the loader recorded the method's slot kinds.
fn declare_frame(frame: &mut Frame, module: &Module, id: MethodId) {
    let Some((args, locals)) = module.method_slot_kinds(id) else {
        return;
    };
    for (value, &code) in frame.args.iter_mut().zip(args) {
        if depends_on_slot_kind(value) {
            let taken = core::mem::replace(value, Value::Null);
            *value = stored_at_slot_kind(Some(SlotKind::from_code(code)), taken);
        }
    }
    frame.locals.clear();
    frame.locals.extend(locals.iter().map(|&code| zero_of_slot_kind(SlotKind::from_code(code))));
}

/// The slot an indirect load through `location` reads: the location itself, or for a pointer that
/// pointer arithmetic moved along a pinned array by whole elements, the element it reached (ECMA-334
/// 2nd ed 25.6 and 25.5.6). An array whose elements are packed is read through its bytes
/// ([`load_from_packed_element`]) before this. A pointer moved by a number of bytes that is not a
/// whole number of elements of any other array, or along an array whose element width is not known,
/// is refused; so is one moved inside a pinned string's UTF-16 code unit, and one moved into a local
/// or an argument, where it is read.
fn load_slot(vm: &Vm, location: Location, opcode: Opcode) -> Result<Location, Trap> {
    match location {
        Location::StringChar { byte_offset, .. } if byte_offset % 2 != 0 => Err(Trap::Unsupported(opcode)),
        Location::Element { array, index, byte_offset } if byte_offset != 0 => {
            let width = vm
                .heap()
                .array_element_width(array)
                .and_then(|width| i64::try_from(width).ok())
                .filter(|&width| width != 0)
                .ok_or(Trap::Unsupported(opcode))?;
            let displacement = element_displacement(byte_offset);
            if displacement % width != 0 {
                return Err(Trap::Unsupported(opcode));
            }
            // A step back reaches the element before, and one before the first element no element.
            let index = i64::try_from(index)
                .ok()
                .and_then(|index| index.checked_add(displacement / width))
                .and_then(|index| usize::try_from(index).ok())
                .ok_or(Trap::Unsupported(opcode))?;
            Ok(Location::Element { array, index, byte_offset: 0 })
        }
        Location::StaticBytes { slot, byte_offset: 0 } => Ok(Location::Static { slot }),
        Location::StaticBytes { .. } => Err(Trap::Unsupported(opcode)),
        other => Ok(other),
    }
}

/// The bits of a slot that holds a number, and how many bytes its value occupies: 4 for an `int32`
/// (to which a slot widens every narrower integer, as a load from a 1- or 2-byte location does,
/// ECMA-335 6th ed I.12.1.2) or a `float32`, 8 for an `int64` or a `float64`, and `native` for a
/// native int, the size the target maps it to (ECMA-335 6th ed I.12.1.1).
fn scalar_slot_bits(value: &Value, native: usize) -> Option<(u64, usize)> {
    match *value {
        Value::Int32(n) => Some((u64::from(n as u32), 4)),
        Value::Int64(n) => Some((n as u64, 8)),
        Value::NativeInt(n) => Some((n as u64, native)),
        #[cfg(feature = "float")]
        Value::Single(f) => Some((u64::from(f.to_bits()), 4)),
        #[cfg(feature = "float")]
        Value::Float(f) => Some((f.to_bits(), 8)),
        _ => None,
    }
}

/// Reads an `ldind.<type>` at the address of a VALUE slot (a local, argument, field, element or
/// static) holding `value`; a read through a pointer moved inside a slot is refused before it gets
/// here. ECMA-335 6th ed III.3.42 loads a value of the instruction's type from the address, and
/// ECMA-334 2nd ed 25.4 allows converting `&variable` to another pointer type and points a `byte*` at
/// the variable's lowest-addressed byte. A local or an argument whose declared kind the loader
/// recorded is read through [`load_from_declared_slot`] before this, and an array whose elements are
/// packed through [`load_from_packed_element`]. Any other slot holds a typed value rather than bytes,
/// so a read answers only where the bytes it names are the value's own bits, taken in little-endian
/// order, the byte order of every target this runs on (ECMA-335 6th ed I.12.6.3 leaves it to the
/// target):
///
/// - a read of the slot's own type is its value, and a read of another integer type as wide as the
///   slot's integer value is its bits (`*(uint*)&i` of an `int`; `*(long*)&p` of a pointer where the
///   target's native int is 8 bytes, ECMA-335 6th ed I.12.1.1);
/// - a `byte`- or `short`-wide read of an `int32` or a native int is its lowest-addressed byte or two;
/// - at a local's or an argument's own address (`variable`), and nowhere else, an `int32` read as a
///   `float32` and an `int64` read as a `float64` are the floating values with those bits, and each of
///   those floating values read as the integer of its own width is its bits (`*(float*)&bits` of a
///   `uint`, `*(ulong*)&d` of a `double`).
///
/// Every other read is refused: a number read as another kind at a field, a static, an array element
/// or a string's code unit; a narrower read of an `int64`; an `int32`-wide read of a native int wider
/// than four bytes; a
/// floating value read at another width, as a local or an argument may hold a floating value in a
/// representation other than its nominal type (ECMA-335 6th ed I.12.1.3 and III.1.1.1); and a read
/// past the slot's value or past `declared`, the slot's width when the model records it (an element
/// of a typed value array, and a pinned string's UTF-16 code unit, 2 bytes, ECMA-335 6th ed
/// III.1.1.3; an `ldind` through a pinned string reads its bytes before this,
/// [`load_from_string_chars`]). A
/// local, an argument, a static and a field record no width here, and a narrower integer in one is
/// held widened to `int32` (ECMA-335 6th ed I.12.1.2), so a read wider than a `byte` or `short`
/// variable but inside its slot answers from the widened value. Past a local, an argument or a static
/// the bytes it reads are not the variable's; past a field they are other fields of its struct or
/// object (in a type of sequential layout, the fields declared after it, ECMA-335 6th ed II.10.1.2),
/// which such a read does not see. A value that is not a number passes through unchanged only to the read
/// that moves it whole: any value to `ldobj` (ECMA-335 6th ed III.4.13), an object reference or a
/// pointer to `ldind.ref`, and a pointer to `ldind.i` (ECMA-335 6th ed III.3.42 and I.12.1.1). A
/// numeric read of a struct, a reference or a pointer is refused, as is `ldind.r4` or `ldind.r8` on a
/// build without floating point.
fn load_from_slot(value: Value, opcode: Opcode, declared: Option<usize>, variable: bool) -> Result<Value, Trap> {
    load_from_slot_at(value, opcode, crate::native_pointer_size() as usize, declared, variable)
}

/// Whether `slot` is a local's or an argument's own address, the one slot at which [`load_from_slot`]
/// reads a number as another kind of number.
fn is_variable_address(slot: &Location) -> bool {
    matches!(
        slot,
        Location::Local { .. }
            | Location::Arg { .. }
            | Location::LocalBytes { byte_offset: 0, .. }
            | Location::ArgBytes { byte_offset: 0, .. }
    )
}

/// The storage width of `slot` when the model records it: an array element's, when the array
/// records its element width, and a pinned string's UTF-16 code unit, 2 bytes (ECMA-335 6th ed
/// III.1.1.3: a char occupies 2 bytes and is a UTF-16 code unit).
fn declared_width(vm: &Vm, slot: &Location) -> Option<usize> {
    match slot {
        Location::Element { array, .. } => vm.heap().array_declared_element_width(*array),
        Location::StringChar { .. } => Some(2),
        _ => None,
    }
}

/// [`load_from_slot`] for a target whose native int is `native` bytes wide: `ldind.i` reads a native
/// int, as wide as the target maps it (ECMA-335 6th ed I.12.1.1 and III.3.42), and decodes four bytes
/// as [`stack_loaded_value_at`] does.
fn load_from_slot_at(value: Value, opcode: Opcode, native: usize, declared: Option<usize>, variable: bool) -> Result<Value, Trap> {
    // `ldobj` copies a whole value of any type and `ldind.ref` an object reference (ECMA-335 6th ed
    // III.4.13 and III.3.42), so neither has a width of its own. A pointer is as wide as an object
    // reference (ECMA-335 6th ed I.12.1.1), so `ldind.ref` of a pointer variable is its own value too.
    // `ldind.r4` and `ldind.r8` have no width on a build without floating point, and are refused there.
    let Some(width) = indirect_width_at(opcode, native) else {
        return match (opcode, &value) {
            (Opcode::Ldobj, _)
            | (Opcode::LdindRef, Value::Object(_) | Value::Null | Value::ByRef(_) | Value::NativeInt(_)) => Ok(value),
            _ => Err(Trap::Unsupported(opcode)),
        };
    };
    // A pointer variable read as a native int is its own value (ECMA-335 6th ed I.12.1.1.1 treats an
    // unmanaged pointer as a native unsigned int). Any other value that is not a number (a struct, a
    // reference) is refused for a numeric read.
    let Some((bits, size)) = scalar_slot_bits(&value, native) else {
        return match (opcode, &value) {
            (Opcode::LdindI, Value::ByRef(_) | Value::Null) => Ok(value),
            _ => Err(Trap::Unsupported(opcode)),
        };
    };
    // A slot read here records no declared type (a local or an argument with one is read through
    // [`load_from_declared_slot`]), so the value's tag stands in for it: an `Int32` holds at most 4
    // bytes, an `Int64` 8, a native int the target's native size. An array that records its element
    // width bounds the read by it too: a packed `byte[]` element is one byte, though its value is held
    // widened to int32 (ECMA-335 6th ed I.12.1.2: a load from a 1- or 2-byte location expands to 4
    // bytes). Every answer below is inside both bounds.
    if width > declared.map_or(size, |declared| declared.min(size)) {
        return Err(Trap::Unsupported(opcode));
    }
    if !answers_from_slot_value(&value, opcode, width, size, variable) {
        return Err(Trap::Unsupported(opcode));
    }
    // A native int variable read as a native int is read at its own width (ECMA-335 6th ed I.12.1.1
    // and III.3.42), so its value is kept in the form it holds rather than decoded from its bytes,
    // which on a 4-byte target cannot say which form was stored.
    if opcode == Opcode::LdindI && matches!(value, Value::NativeInt(_)) {
        return Ok(value);
    }
    stack_loaded_value_at(opcode, bits.to_le_bytes(), native)
}

/// Whether an `ldind` `width` bytes wide answers from the bits of `value`, a number `size` bytes wide
/// held at a local's or an argument's own address when `variable` is set; [`load_from_slot`] lists the
/// reads that answer. A read of a number as another kind answers only at such an address and only at
/// the value's own width; a narrower read of the same kind only takes a byte or two of an `int32` or a
/// native int, and a floating value is read only at its own width.
fn answers_from_slot_value(value: &Value, opcode: Opcode, width: usize, size: usize, variable: bool) -> bool {
    if matches!(opcode, Opcode::LdindR4 | Opcode::LdindR8) != holds_floating(value) {
        return variable
            && match (value, opcode) {
                (Value::Int32(_), Opcode::LdindR4) | (Value::Int64(_), Opcode::LdindR8) => true,
                #[cfg(feature = "float")]
                (Value::Single(_), Opcode::LdindI4 | Opcode::LdindU4) | (Value::Float(_), Opcode::LdindI8) => true,
                _ => false,
            };
    }
    width == size
        || matches!(opcode, Opcode::LdindI1 | Opcode::LdindU1 | Opcode::LdindI2 | Opcode::LdindU2)
            && matches!(value, Value::Int32(_) | Value::NativeInt(_))
}

/// Whether `value` is a floating value, a `float32` or a `float64`.
fn holds_floating(value: &Value) -> bool {
    match value {
        #[cfg(feature = "float")]
        Value::Single(_) | Value::Float(_) => true,
        _ => false,
    }
}

/// Reads the whole value a managed pointer names. Raw stack bytes and other locations
/// without a readable whole value are refused; they do not represent a null reference.
fn read_byref(frames: &[Frame], vm: &Vm, location: Location, opcode: Opcode) -> Result<Value, Trap> {
    read_location_value(frames, vm, location).ok_or(Trap::Unsupported(opcode))
}

/// The underlying integer of an enum value at a managed pointer this frame can reach (a
/// local/argument of any live frame, or a heap field/element/static) -- for Enum.ToString.
#[cfg(feature = "bcl")]
fn read_enum_value(
    frame: &Frame,
    frame_index: usize,
    callers: &[Frame],
    vm: &Vm,
    location: Location,
) -> Option<i64> {
    match read_constrained_receiver(frame, frame_index, callers, vm, &location)? {
        Value::Int32(n) => Some(i64::from(n)),
        Value::Int64(n) => Some(n),
        _ => None,
    }
}

/// `value`, an enum's value as its storage holds it, with the sign of the enum's declared underlying
/// type `cast` (ECMA-335 6th ed II.14.3): an unsigned type's bits zero-extended, so a uint enum's
/// 0xFFFFFFFF is 4294967295, and a signed type's sign-extended. An eight-byte type leaves it as it is.
#[cfg(feature = "bcl")]
fn with_declared_sign(cast: CastPrim, value: i64) -> i64 {
    match cast {
        CastPrim::I1 => i64::from(value as i8),
        CastPrim::U1 | CastPrim::Bool => i64::from(value as u8),
        CastPrim::I2 => i64::from(value as i16),
        CastPrim::U2 | CastPrim::Char => i64::from(value as u16),
        CastPrim::I4 => i64::from(value as i32),
        CastPrim::U4 => i64::from(value as u32),
        _ => value,
    }
}

/// Refuses, as `callvirt`, a `constrained.` call whose receiver pointer names a variable, a field, an
/// element or a static the loader recorded with another storage than the constraint type's: a pointer
/// cast from it (`((E*)&n)->ToString()` of a `long n`), whose bytes this interpreter does not read as the
/// constraint type. Where nothing is recorded the call goes on as before. `frame` runs at `frame_index`
/// above `callers`.
#[allow(clippy::too_many_arguments)]
fn refuse_cast_receiver(
    frame: &Frame, frame_index: usize, callers: &[Frame], vm: &Vm, module: &Module, location: &Location, asm: u8, constraint: Token,
) -> Result<(), Trap> {
    if let Location::Element { array, index, byte_offset } = *location
        && array_element_index(vm, array, index, byte_offset).is_none()
    {
        return Err(Trap::Unsupported(Opcode::Callvirt));
    }
    let method_of = |index: usize| if index == frame_index { Some(frame.method) } else { callers.get(index).map(|caller| caller.method) };
    match (declared_storage_at(&method_of, vm, module, location), obj_storage(module, asm, constraint)) {
        (Some(declared), Some(own)) if declared != own => Err(Trap::Unsupported(Opcode::Callvirt)),
        _ => Ok(()),
    }
}

/// The whole value a `constrained.` receiver's managed pointer names, for the III.2.1 box below --
/// the same location set [`read_enum_value`] reads (live frames' locals and arguments, a heap
/// field, an array element, a static, a box interior), widened from an enum's integer to any value.
///
/// A by-reference parameter or a struct's `this` can name a caller's storage. The
/// current frame is borrowed separately from its callers while an instruction runs.
fn read_constrained_receiver(
    frame: &Frame,
    frame_index: usize,
    callers: &[Frame],
    vm: &Vm,
    location: &Location,
) -> Option<Value> {
    let containing_frame = |index| {
        if index == frame_index { Some(frame) } else { callers.get(index) }
    };
    match location {
        Location::Local { frame: f, slot } => containing_frame(*f)?.locals.get(*slot).cloned(),
        Location::Arg { frame: f, slot } => containing_frame(*f)?.args.get(*slot).cloned(),
        Location::Field { object, slot } => vm.heap().instance_field(*object, *slot),
        Location::Element {
            array,
            index,
            byte_offset,
        } => array_element_index(vm, *array, *index, *byte_offset).and_then(|element| vm.heap().array_get(*array, element)),
        Location::Static { slot } => vm.static_field(*slot),
        Location::LocalBytes { frame: f, slot, byte_offset: 0 } => containing_frame(*f)?.locals.get(*slot).cloned(),
        Location::ArgBytes { frame: f, slot, byte_offset: 0 } => containing_frame(*f)?.args.get(*slot).cloned(),
        Location::StaticBytes { slot, byte_offset: 0 } => vm.static_field(*slot),
        Location::Boxed { object } => vm.heap().boxed_value(*object),
        Location::Nested { base, slot } => {
            match read_constrained_receiver(frame, frame_index, callers, vm, base) {
                Some(Value::Struct(fields)) => fields.get(*slot as usize).cloned(),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The registry ids the recognizers below test against.
///
/// `const` rather than a call in each arm, so the FNV of the name is evaluated by language rule
/// instead of by the optimizer's goodwill: these run on the `newobj` / `callvirt` path, and hashing
/// a name per instruction is not a cost worth leaving to chance.
#[cfg(feature = "bcl")]
mod intrinsic_ids {
    use super::intrinsic_id;

    pub(super) const OBJECT_TO_STRING: u32 = intrinsic_id("object_to_string");
    pub(super) const INTERLOCKED_COMPARE_EXCHANGE: u32 = intrinsic_id("interlocked_compare_exchange");
    pub(super) const STRING_CTOR_CHAR_PTR: u32 = intrinsic_id("string_ctor_char_ptr");
    pub(super) const STRING_CTOR_CHAR_PTR_RANGE: u32 = intrinsic_id("string_ctor_char_ptr_range");
    pub(super) const STRING_CTOR_CHAR_ARRAY: u32 = intrinsic_id("string_ctor_char_array");
    pub(super) const STRING_CTOR_CHAR_ARRAY_RANGE: u32 = intrinsic_id("string_ctor_char_array_range");
    pub(super) const STRING_CTOR_CHAR_REPEAT: u32 = intrinsic_id("string_ctor_char_repeat");
}

/// Whether `method` is the `Object.ToString` intrinsic, so a `constrained.` Enum.ToString
/// can be rendered as the constant name rather than the boxed value's text.
#[cfg(feature = "bcl")]
fn is_object_to_string(module: &Module, method: MethodId) -> bool {
    module.intrinsic_id(method) == Some(intrinsic_ids::OBJECT_TO_STRING)
}

/// Whether `method` is the `Interlocked.CompareExchange` intrinsic, so its first argument is
/// passed as the raw managed pointer (to write back through it) rather than dereferenced like
/// an ordinary intrinsic's by-ref argument. Always `false` without the `bcl` intrinsics.
#[cfg(feature = "bcl")]
fn is_compare_exchange(module: &Module, method: MethodId) -> bool {
    module.intrinsic_id(method) == Some(intrinsic_ids::INTERLOCKED_COMPARE_EXCHANGE)
}

#[cfg(not(feature = "bcl"))]
fn is_compare_exchange(_module: &Module, _method: MethodId) -> bool {
    false
}

/// Which String char*-constructor `method` is bound to, if either: `Some(false)` for the
/// terminator-scanning `String(char*)`, `Some(true)` for `String(char*, int, int)`. The
/// two intrinsics are identity anchors -- the loader binds them so `newobj` resolves, and
/// the interpreter routes both through [`Flow::NewString`] (which can reach a cross-frame
/// stackalloc buffer the intrinsic ABI cannot). Always `None` without the `bcl` intrinsics, where
/// no String constructor is bound to one.
#[cfg(feature = "bcl")]
fn string_pointer_ctor_form(module: &Module, method: MethodId) -> Option<bool> {
    match module.intrinsic_id(method)? {
        intrinsic_ids::STRING_CTOR_CHAR_PTR => Some(false),
        intrinsic_ids::STRING_CTOR_CHAR_PTR_RANGE => Some(true),
        _ => None,
    }
}

#[cfg(not(feature = "bcl"))]
fn string_pointer_ctor_form(_module: &Module, _method: MethodId) -> Option<bool> {
    None
}

/// Which of the three SAFE String constructors `method` is bound to, if any.
///
/// Separate from [`string_pointer_ctor_form`] because these need nothing the pointer forms need: an
/// array reference and an integer are ordinary values, reachable where `newobj` is handled, so
/// there is no cross-frame buffer to chase and no [`Flow`] to route through. They are anchors for
/// the other half of the same reason -- a constructor cannot hand back an object other than the one
/// under construction, and a string built from arguments has to be produced by the instruction.
#[cfg(feature = "bcl")]
fn string_value_ctor_form(module: &Module, method: MethodId) -> Option<StringValueCtor> {
    match module.intrinsic_id(method)? {
        intrinsic_ids::STRING_CTOR_CHAR_ARRAY => Some(StringValueCtor::WholeArray),
        intrinsic_ids::STRING_CTOR_CHAR_ARRAY_RANGE => Some(StringValueCtor::ArrayRange),
        intrinsic_ids::STRING_CTOR_CHAR_REPEAT => Some(StringValueCtor::Repeat),
        _ => None,
    }
}

/// The three SAFE String constructors, by the shape of their arguments.
#[cfg(feature = "bcl")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum StringValueCtor {
    /// `String(char[])` -- every code unit of the array.
    WholeArray,
    /// `String(char[], int, int)` -- a range of it.
    ArrayRange,
    /// `String(char, int)` -- one code unit, repeated.
    Repeat,
}

/// Build the string one of the three SAFE String constructors names, from its popped arguments.
///
/// Every out-of-range case is a REFUSAL rather than a clamp. A `String(chars, start, length)` whose
/// range runs past the array is a caller mistake, and answering it with a shorter string hands back
/// something well formed and wrong -- the far end has no way to tell it from a string that was
/// always that length, which is the same reasoning the frame codec refuses an over-long payload on.
#[cfg(feature = "bcl")]
fn build_string_value(
    vm: &mut Vm,
    args: &[Value],
    form: StringValueCtor,
) -> Result<crate::object::ObjectRef, Trap> {
    let units = match form {
        StringValueCtor::Repeat => {
            let unit = match args.first() {
                Some(&Value::Int32(unit)) => unit as u16,
                _ => return Err(Trap::TypeMismatch(Opcode::Newobj)),
            };
            let count = ctor_int_arg(args, 1)?;
            let count = usize::try_from(count).map_err(|_| Trap::ArgumentOutOfRange(1))?;
            alloc::vec![unit; count]
        }
        StringValueCtor::WholeArray | StringValueCtor::ArrayRange => {
            let reference = match args.first() {
                Some(&Value::Object(reference)) => reference,
                Some(&Value::Null) => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Newobj)),
            };
            if vm.heap().array_element_type(reference).is_none() {
                return Err(Trap::TypeMismatch(Opcode::Newobj));
            }
            let (start, length) = match form {
                StringValueCtor::ArrayRange => {
                    let start = usize::try_from(ctor_int_arg(args, 1)?)
                        .map_err(|_| Trap::ArgumentOutOfRange(1))?;
                    let length = usize::try_from(ctor_int_arg(args, 2)?)
                        .map_err(|_| Trap::ArgumentOutOfRange(2))?;
                    (start, length)
                }
                _ => (0, vm.heap().array_len(reference).unwrap_or(0)),
            };
            vm.heap()
                .array_utf16_units(reference, start, length)
                .ok_or(Trap::ArgumentOutOfRange(2))?
        }
    };
    Ok(vm.heap_mut().alloc_string(&units)?)
}

/// The `int32` at `slot` of a popped constructor argument list.
fn ctor_int_arg(args: &[Value], slot: usize) -> Result<i32, Trap> {
    match args.get(slot) {
        Some(&Value::Int32(value)) => Ok(value),
        _ => Err(Trap::TypeMismatch(Opcode::Newobj)),
    }
}

/// Stores `value` at `slot`, growing `slots` with `Null` placeholders to reach it.
fn set_slot(slots: &mut Vec<Value>, slot: usize, value: Value) {
    while slots.len() <= slot {
        slots.push(Value::Null);
    }
    slots[slot] = value;
}

/// Builds a delegate invocation's arguments: the bound target (if any) ahead of the
/// shared parameters -- an instance method receives `this`, a static method does not.
fn delegate_call_args(target: &Value, params: &[Value]) -> Vec<Value> {
    let mut call_args = Vec::with_capacity(params.len() + 1);
    if !matches!(target, Value::Null) {
        call_args.push(target.clone());
    }
    call_args.extend_from_slice(params);
    call_args
}

/// The argument list for invoking a delegate's bound `method` on `target`: `delegate_call_args`
/// prepends the bound target as `this`, then -- for a delegate over a VALUE-TYPE instance method,
/// whose receiver was boxed at delegate construction -- the box reference is replaced with a
/// managed pointer INTO the box. The value type's own method takes `this` as a managed pointer
/// (ECMA-335 III.4.2, the `unbox` a `box` callvirt-ed to its value type's method reduces to), so
/// the body reads the boxed value through `this` (`ldarg.0; ldfld` / `ldind`). This mirrors the
/// ordinary boxed-callvirt path; a reference-type target keeps its object `this`.
fn delegate_receiver_args(
    module: &Module,
    vm: &Vm,
    method: MethodId,
    target: &Value,
    params: &[Value],
) -> Vec<Value> {
    let mut args = delegate_call_args(target, params);
    if module.method_declares_value_type(method) {
        if let Some(&Value::Object(reference)) = args.first() {
            if vm.heap().boxed_type_token(reference).is_some() {
                args[0] = Value::ByRef(Location::Boxed { object: reference });
            }
        }
    }
    args
}

/// Writes `value` into field `slot` of the value-type instance at `location`,
/// materializing it from `shape` (the declaring type's zero fields) if it is not yet a
/// struct. Reads the container, sets the field, writes it back -- so it serves a frame
/// local/arg or a heap/static struct alike.
fn write_field_at(
    frames: &mut [Frame],
    vm: &mut Vm,
    location: Location,
    slot: u32,
    shape: &[Value],
    value: Value,
) -> Result<(), Trap> {
    let mut container = read_location_value(frames, vm, location.clone()).unwrap_or(Value::Null);
    if !matches!(container, Value::Struct(_)) {
        container = Value::Struct(shape.to_vec().into_boxed_slice());
    }
    if let Value::Struct(fields) = &mut container {
        if let Some(target) = fields.get_mut(slot as usize) {
            *target = value;
        }
    }
    write_location_value(frames, vm, location, container)
}

/// The trap for a `call`, `callvirt`, `newobj`, `jmp`, `ldftn` or `ldvirtftn` whose token resolves
/// to no method.
///
/// A token the loader marked as reaching a generic instantiation it did not lower raises a
/// catchable `MissingMethodException` naming the instantiation, with why it was not lowered where
/// the loader recorded that ([`Module::unlowered_generic_name`]), as .NET raises one for a call it
/// cannot bind. Any other token traps [`Trap::UnresolvedCall`].
fn unresolved_call(module: &Module, asm: u8, token: Token) -> Trap {
    if !module.is_unlowered_generic(asm, token) {
        return Trap::UnresolvedCall(token);
    }
    let message = match module.unlowered_generic_name(asm, token) {
        Some(message) => String::from(message),
        None => alloc::format!(
            "Method not found: the generic instantiation at token 0x{:08X}. Lamella did not lower it.",
            token.0
        ),
    };
    Trap::Refused(crate::trap::RefusedWith::MissingMethod, Cow::Owned(message))
}

/// The trap for a field instruction whose token resolves to no field.
///
/// A token the loader marked as reaching a generic instantiation it did not lower raises a catchable
/// `MissingFieldException` naming the field, with why it was not lowered, as [`unresolved_call`] does
/// for a call and as .NET raises one for a field it cannot bind. Any other token traps
/// [`Trap::UnresolvedField`].
fn unresolved_field(module: &Module, asm: u8, token: Token) -> Trap {
    if !module.is_unlowered_generic(asm, token) {
        return Trap::UnresolvedField(token);
    }
    let message = match module.unlowered_generic_name(asm, token) {
        Some(message) => String::from(message),
        None => alloc::format!(
            "Field not found: the generic instantiation at token 0x{:08X}. Lamella did not lower it.",
            token.0
        ),
    };
    Trap::Refused(crate::trap::RefusedWith::MissingField, Cow::Owned(message))
}

/// Dispatches a `call` of a bodyless `[DllImport]` method: takes the call's arguments
/// off the evaluation stack, marshals them per the target's recorded signature shape
/// (integers ride a 64-bit scalar slot; a managed string becomes a NUL-terminated byte
/// buffer -- exact for the ASCII range an ANSI import reads), invokes the host seam, and
/// pushes the mapped return. Without an installed host (the device tiers) the call traps
/// as unresolved, exactly as if the import did not exist.
fn pinvoke_call(
    frame: &mut Frame,
    vm: &mut Vm,
    target: &crate::module::PInvokeTarget,
    token: Token,
) -> Result<Flow, Trap> {
    use crate::module::{PInvokeParam, PInvokeReturn};
    let host = vm.pinvoke_host().ok_or(Trap::UnresolvedCall(token))?;
    let args = frame.take_args(target.params.len() as u16)?;
    let mut marshaled = Vec::with_capacity(args.len());
    for (value, kind) in args.iter().zip(&target.params) {
        marshaled.push(match kind {
            PInvokeParam::Scalar => PInvokeArg::Scalar(match value {
                Value::Int32(n) => i64::from(*n),
                Value::Int64(n) | Value::NativeInt(n) => *n,
                _ => return Err(Trap::TypeMismatch(Opcode::Call)),
            }),
            PInvokeParam::AnsiString => match value {
                Value::Null => PInvokeArg::Scalar(0),
                Value::Object(reference) => {
                    let units = vm
                        .heap()
                        .as_string(*reference)
                        .ok_or(Trap::TypeMismatch(Opcode::Call))?;
                    let mut bytes = String::from_utf16_lossy(&units).into_bytes();
                    bytes.push(0);
                    PInvokeArg::Bytes(bytes)
                }
                _ => return Err(Trap::TypeMismatch(Opcode::Call)),
            },
        });
    }
    let result = host(target, &mut marshaled).map_err(|()| Trap::UnresolvedCall(token))?;
    match target.returns {
        PInvokeReturn::Void => {}
        PInvokeReturn::Int32 => frame.stack.push(Value::Int32(result as i32)),
    }
    Ok(Flow::Next)
}

/// Pushes the not-yet-run `.cctor` frame of `method`'s declaring type, if any, on top of
/// the just-pushed callee frame -- the initializer runs to completion first, returns
/// void, and the callee proceeds (II.10.5.3: first method entry triggers initialization).
fn push_pending_cctor(
    frames: &mut Vec<Frame>,
    module: &Module,
    vm: &mut Vm,
    method: MethodId,
) -> Result<(), Trap> {
    let Some(type_id) = module.method_type(method) else {
        return Ok(());
    };
    let Some(cctor) = vm.take_pending_cctor(module, type_id)
                    .map_err(Trap::TypeInitializationFailed)? else {
        return Ok(());
    };
    if frames.len() >= MAX_CALL_DEPTH {
        return Err(Trap::CallStackOverflow);
    }
    let frame = new_frame(vm, module, cctor, Vec::new())?;
    frames.push(frame);
    Ok(())
}

/// Runs the not-yet-run `.cctor` of `method`'s declaring type as a NESTED session -- for
/// the inline-intrinsic dispatch, which pushes no frame of its own (the same pattern the
/// reflective intrinsics use to run attribute constructors).
///
/// The caller's `frames` are parked for the run, and the call's `args`, which the call has
/// already taken off the caller's stack, go back onto it until the run ends: a collection inside
/// the constructor marks and relocates both, and `args` come back holding the new positions.
/// Nothing is parked when no constructor runs.
fn run_pending_cctor_nested(
    module: &Module,
    vm: &mut Vm,
    method: MethodId,
    frames: &mut Vec<Frame>,
    args: &mut Vec<Value>,
) -> Result<(), Trap> {
    let Some(type_id) = module.method_type(method) else {
        return Ok(());
    };
    let Some(cctor) = vm.take_pending_cctor(module, type_id)
                    .map_err(Trap::TypeInitializationFailed)? else {
        return Ok(());
    };
    #[cfg(feature = "gc")]
    let count = args.len();
    #[cfg(feature = "gc")]
    {
        if let Some(caller) = frames.last_mut() {
            caller.stack.append(args);
        }
        vm.park_frames(core::mem::take(frames));
    }
    #[cfg(not(feature = "gc"))]
    let _ = (frames, args);
    let ran = Session::new(module, cctor, Vec::new()).and_then(|mut session| session.run(module, vm));
    #[cfg(feature = "gc")]
    {
        *frames = vm.unpark_frames();
        if let Some(caller) = frames.last_mut() {
            let at = caller.stack.len().saturating_sub(count);
            args.extend(caller.stack.drain(at..));
        }
    }
    ran.map(|_| ())
}

fn new_frame(vm: &mut Vm, module: &Module, id: MethodId, args: Vec<Value>) -> Result<Frame, Trap> {
    match module.method_kind(id) {
        Some(MethodKind::Managed) => {
            let mut frame = vm.frame_pool.revive(id, MethodContext::of(module, id), args);
            frame.prepare_for_handlers(eh_clause_count(module, id));
            declare_frame(&mut frame, module, id);
            Ok(frame)
        }
        _ => Err(Trap::NoSuchMethod(id)),
    }
}

/// [`new_frame`] without a [`Vm`] (no pool): the session/thread ENTRY frame, one per
/// program start -- the per-call recycling happens in `advance`, which always has the `Vm`.
fn new_frame_cold(module: &Module, id: MethodId, args: Vec<Value>) -> Result<Frame, Trap> {
    match module.method_kind(id) {
        Some(MethodKind::Managed) => {
            let mut frame = Frame::new(id, MethodContext::of(module, id), args);
            frame.prepare_for_handlers(eh_clause_count(module, id));
            declare_frame(&mut frame, module, id);
            Ok(frame)
        }
        _ => Err(Trap::NoSuchMethod(id)),
    }
}

/// [`Frame::take_args`] into a POOLED buffer: the top `count` stack values move (in
/// declaration order) into a recycled `Vec`, so a warm call site allocates nothing.
fn take_args_pooled(frame: &mut Frame, vm: &mut Vm, count: u16) -> Result<Vec<Value>, Trap> {
    let split = frame
        .stack
        .len()
        .checked_sub(count as usize)
        .ok_or(Trap::StackUnderflow)?;
    let mut args = vm.frame_pool.take_values();
    args.extend(frame.stack.drain(split..));
    Ok(args)
}

/// The CIL of a managed method, read from the module: on each step for a body a frame cannot keep
/// (decoded instructions, or code bytes the module owns in RAM), since a frame does not borrow the
/// module. Errors if `id` names an intrinsic or no method.
///
/// The view is the `code_loading` level's fetch source: decoded instructions (indexed by an
/// instruction-count ip) by default, or -- under `code-in-place` -- the flash-resident code BYTES,
/// decoded one instruction per step with the ip a CIL byte offset. `advance` is the only fetch
/// site, and [`step`] moves ip/target/len around as opaque values, so both domains run the same
/// dispatcher unchanged.
/// Whether a frame's `ip` is a CIL BYTE OFFSET rather than an instruction INDEX.
///
/// # Why this is exported at all
///
/// **THE DOMAIN IS DECIDED BY A FEATURE THIS CRATE DECLARES, AND A CONSUMER CANNOT SEE IT.** A
/// cargo feature is not visible to `cfg` outside the crate that declares it, so `lamella-dap`
/// cannot write `#[cfg(feature = "code-in-place")]` and cannot ask any other way. Without this it
/// has to ASSUME a domain, and the assumption is wrong in exactly one configuration.
///
///
/// This is the same repair as [`Module::method_instructions`] one level up: **ask the crate that
/// owns the condition.**
pub const IP_IS_IL_OFFSET: bool = cfg!(feature = "code-in-place");

#[cfg(not(feature = "code-in-place"))]
fn method_code(module: &Module, id: MethodId) -> Result<&[Instruction], Trap> {
    module.method_body(id).map(|body| &body.code[..]).ok_or(Trap::NoSuchMethod(id))
}
#[cfg(feature = "code-in-place")]
fn method_code(module: &Module, id: MethodId) -> Result<&[u8], Trap> {
    module.method_code_bytes(id).ok_or(Trap::NoSuchMethod(id))
}

/// The exception-handling clauses of a managed method. Region units match the active ip domain
/// (instruction indices by default; CIL byte offsets under `code-in-place`), so the unwinder's
/// membership tests and handler entries need no translation either way.
#[cfg(not(feature = "code-in-place"))]
fn method_handlers(module: &Module, id: MethodId) -> Result<&[EhClause], Trap> {
    module.method_body(id).map(|body| &body.handlers[..]).ok_or(Trap::NoSuchMethod(id))
}
#[cfg(feature = "code-in-place")]
fn method_handlers(module: &Module, id: MethodId) -> Result<Vec<EhClause>, Trap> {
    module.method_eh(id).ok_or(Trap::NoSuchMethod(id))
}

/// How many exception clauses a managed method declares, read without materializing them -- the
/// most caught exceptions its frame can hold at once, since each is keyed by its handler.
#[cfg(not(feature = "code-in-place"))]
fn eh_clause_count(module: &Module, id: MethodId) -> usize {
    module.method_body(id).map_or(0, |body| body.handlers.len())
}
#[cfg(feature = "code-in-place")]
fn eh_clause_count(module: &Module, id: MethodId) -> usize {
    module.method_eh_count(id)
}

/// A reserved type id for objects whose type is external to this module: a runtime-fault
/// exception (divide-by-zero, etc.) or a `new` of an external BCL type whose constructor
/// is an intrinsic (e.g. `System.Exception`). No loaded type has this id, so it has no
/// field layout or vtable, and `sig_dispatch` finds nothing for it (callvirt falls back to
/// the bound intrinsic). A runtime-fault exception records its base-chain tag vector
/// separately (`Vm::set_exception_chain`) so `catch` still matches it by type, and its leaf
/// `Type` handle separately again (`Vm::set_exception_type`) so `GetType()` can answer.
///
const EXTERNAL_TYPE_ID: u32 = u32::MAX;

/// How many wrapped exceptions an unhandled-exception report follows below the one that escaped.
#[cfg(feature = "exceptions")]
const MAX_REPORTED_INNER_EXCEPTIONS: usize = 8;

/// What the VES knows about one runtime-raised exception, whose managed type is external to the
/// loaded module and so carries neither a base chain to walk nor a type handle to read.
///
/// Every fact is recorded at the moment the object is minted and is relocated together by the
/// collector, which is why they share one entry rather than one table each.
#[cfg(feature = "exceptions")]
#[derive(Debug)]
struct FaultIdentity {
    /// The base-chain tag vector, leaf-first up to `System.Object` -- what a `catch`'s tag is
    /// tested against, identical to what `Module::exception_base_chain` yields for a managed throw.
    chain: Vec<u32>,
    /// The leaf type's asm-folded `Type` handle, or `None` when that type is not loaded.
    type_handle: Option<u64>,
    /// The leaf type's full name, which an unhandled-exception report prints when the image does
    /// not carry the type: an image trimmed to what `Main` reaches often does not.
    type_name: &'static str,
    /// The method that was executing when the VES raised it, which the report names. `None` for
    /// one raised at a type's first access, which is a wrapper and points at its inner exception.
    raised_in: Option<MethodId>,
}

/// The .NET exception type a catchable runtime fault surfaces as: a default message and the
/// type's base-chain full names leaf-first up to `System.Object`. The handler search needs the
/// WHOLE chain (not just the leaf) so that, e.g., a div-by-zero is caught by
/// `catch (DivideByZeroException)`, `catch (ArithmeticException)`, `catch (Exception)`, or a
/// typeless `catch {}` (== Object) alike. `None` for traps that should still abort (a stack
/// overflow, an unresolved token, malformed CIL, ...). The chains mirror .NET's hierarchy so
/// the tags equal those a managed `throw` of the same type produces.
///
/// The message is a [`Cow`] because one fault names the value that caused it: an unencodable
/// code unit reports WHICH unit and where, as .NET's own message for it does. Every other
/// fault's text is a constant and stays borrowed.
#[cfg(feature = "exceptions")]
fn fault_exception(trap: &Trap) -> Option<(Cow<'static, str>, &'static [&'static str])> {
    const ARITHMETIC: &[&str] = &[
        "System.DivideByZeroException",
        "System.ArithmeticException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const OVERFLOW: &[&str] = &[
        "System.OverflowException",
        "System.ArithmeticException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const NULL_REF: &[&str] = &[
        "System.NullReferenceException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const OUT_OF_MEMORY: &[&str] = &[
        "System.OutOfMemoryException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const INDEX_OOB: &[&str] = &[
        "System.IndexOutOfRangeException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const ARG_OOR: &[&str] = &[
        "System.ArgumentOutOfRangeException",
        "System.ArgumentException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const INVALID_CAST: &[&str] = &[
        "System.InvalidCastException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const ARRAY_MISMATCH: &[&str] = &[
        "System.ArrayTypeMismatchException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const ARGUMENT: &[&str] = &[
        "System.ArgumentException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const SYNC_LOCK: &[&str] = &[
        "System.Threading.SynchronizationLockException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const MISSING_METHOD: &[&str] = &[
        "System.MissingMethodException",
        "System.MissingMemberException",
        "System.MemberAccessException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const MISSING_FIELD: &[&str] = &[
        "System.MissingFieldException",
        "System.MissingMemberException",
        "System.MemberAccessException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const ENCODER_FALLBACK: &[&str] = &[
        "System.Text.EncoderFallbackException",
        "System.ArgumentException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    if let Trap::EncoderFallback {
        char_unknown,
        index,
    } = trap
    {
        return Some((
            Cow::Owned(alloc::format!(
                "Unable to translate Unicode character \\\\u{char_unknown:04X} at index {index} to specified code page."
            )),
            ENCODER_FALLBACK,
        ));
    }
    let (text, chain): (&str, &[&str]) = match trap {
        Trap::DivideByZero => ("Attempted to divide by zero.", ARITHMETIC),
        Trap::NullReference => (
            "Object reference not set to an instance of an object.",
            NULL_REF,
        ),
        Trap::IndexOutOfRange(_) => ("Index was outside the bounds of the array.", INDEX_OOB),
        Trap::ArgumentOutOfRange(_) => (
            "Specified argument was out of the range of valid values.",
            ARG_OOR,
        ),
        Trap::InvalidCast => ("Unable to cast object to the target type.", INVALID_CAST),
        Trap::ArrayTypeMismatch => (
            "Attempted to access an element as a type incompatible with the array.",
            ARRAY_MISMATCH,
        ),
        Trap::InvalidArgument => ("Requested value was not found.", ARGUMENT),
        Trap::Refused(with, message) => {
            return Some((
                message.clone(),
                match with {
                    crate::trap::RefusedWith::Argument => ARGUMENT,
                    crate::trap::RefusedWith::ArrayTypeMismatch => ARRAY_MISMATCH,
                    crate::trap::RefusedWith::InvalidCast => INVALID_CAST,
                    crate::trap::RefusedWith::MissingMethod => MISSING_METHOD,
                    crate::trap::RefusedWith::MissingField => MISSING_FIELD,
                },
            ));
        }
        Trap::SynchronizationLock => (
            "Object synchronization method was called from an unsynchronized block of code.",
            SYNC_LOCK,
        ),
        Trap::Overflow => ("Arithmetic operation resulted in an overflow.", OVERFLOW),
        Trap::OutOfMemory => ("Insufficient memory to continue the execution of the program.", OUT_OF_MEMORY),
        _ => return None,
    };
    Some((Cow::Borrowed(text), chain))
}

/// The base chain of the wrapper a failed type initializer raises, leaf-first to `System.Object`.
#[cfg(feature = "exceptions")]
const TYPE_INIT: &[&str] = &[
    "System.TypeInitializationException",
    "System.SystemException",
    "System.Exception",
    "System.Object",
];

/// If `method` is a type initializer that is unwinding, poisons its type and returns the
/// `TypeInitializationException` to propagate in place of `exception`; otherwise returns
/// `exception` unchanged.
///
/// ECMA-335 II.10.5.3.1 guarantee 3 in two halves, and each half is separately observable:
/// **wrapping** (the constructor's own exception must not escape -- a caller catching
/// `TypeInitializationException`, as the standard tells them to, would miss it) and **poisoning**
/// (the type stays inaccessible, so a later access fails the same way rather than reading statics
/// nobody assigned). `tools/corpus/static-init-throws.cs` scores both, separately, by design.
///
#[cfg(feature = "exceptions")]
fn poison_failed_type(
    module: &Module,
    vm: &mut Vm,
    method: MethodId,
    exception: ObjectRef,
) -> ObjectRef {
    let Some(type_id) = module.method_type(method) else {
        return exception;
    };
    if module.cctor_of_type(type_id) != Some(method) {
        return exception;
    }
    if let Some(existing) = vm.cctor_failure(type_id) {
        return existing;
    }
    let name = module.type_full_name(type_id).unwrap_or("<unknown>");
    let message = alloc::format!("The type initializer for '{name}' threw an exception.");
    let wrapper = vm.heap_mut().alloc_instance(EXTERNAL_TYPE_ID, Vec::new());
    let text = vm.heap_mut().alloc_text(&message);
    vm.set_exception_message(wrapper, text);
    let chain: Vec<u32> = TYPE_INIT
        .iter()
        .copied()
        .map(crate::exception::exception_tag)
        .collect();
    let type_handle = TYPE_INIT.first().and_then(|leaf| module.type_handle_by_name(leaf));
    let type_name = TYPE_INIT.first().copied().unwrap_or_default();
    vm.set_exception_identity(wrapper, FaultIdentity { chain, type_handle, type_name, raised_in: None });
    vm.set_exception_inner(wrapper, exception);
    vm.note_cctor_failed(type_id, wrapper);
    wrapper
}

/// Converts a catchable runtime fault into a thrown exception object (carrying a default
/// message and its base-chain tag vector so `catch` matches it by type), or returns `None`
/// for traps that should still abort execution (a stack overflow, an unresolved token,
/// malformed CIL, ...). `raised_in` is the method that was executing, which an
/// unhandled-exception report names.
#[cfg(feature = "exceptions")]
fn catchable_fault(
    trap: &Trap,
    module: &Module,
    vm: &mut Vm,
    raised_in: Option<MethodId>,
) -> Option<ObjectRef> {
    if let Trap::TypeInitializationFailed(type_id) = *trap {
        return vm.cctor_failure(type_id);
    }
    let (text, chain_names) = fault_exception(trap)?;
    let chain: Vec<u32> = chain_names
        .iter()
        .copied()
        .map(crate::exception::exception_tag)
        .collect();
    let exception = vm.heap_mut().alloc_instance(EXTERNAL_TYPE_ID, Vec::new());
    let message = vm.heap_mut().alloc_text(&text);
    vm.set_exception_message(exception, message);
    let type_handle = chain_names.first().and_then(|leaf| module.type_handle_by_name(leaf));
    let type_name = chain_names.first().copied().unwrap_or_default();
    vm.set_exception_identity(exception, FaultIdentity { chain, type_handle, type_name, raised_in });
    Some(exception)
}

/// Propagates `exception`: searches the call stack from the top for a catch handler
/// whose try region covers the faulting instruction and whose type matches, entering
/// it (eval stack cleared, exception pushed). Frames with no handler are unwound.
/// Returns [`Trap::UnhandledException`] if the stack is exhausted.
fn raise(
    frames: &mut Vec<Frame>,
    module: &Module,
    vm: &mut Vm,
    exception: ObjectRef,
) -> Result<Status, Trap> {
    let Some(frame) = frames.last() else {
        return Err(Trap::UnhandledException(vm.unhandled_detail(module)));
    };
    // A failed initializer propagates at its triggering instruction even though the
    // resume position was rewound so a successful initializer could replay it.
    let fault_ip = frame.fault_ip;
    raise_from(frames, module, vm, exception, 0, fault_ip)
}

/// Searches this frame's handler clauses, from index `from`, for one that catches
/// `exception` -- a type-matching `catch`, or a `filter` whose expression evaluates to
/// true. A filter is evaluated inline: the frame runs its expression and `endfilter`
/// resumes the search here (a rejecting filter continuing from the next clause). With no
/// catcher, the finallys and faults covering the fault run and the frame unwinds.
fn raise_from(
    frames: &mut Vec<Frame>,
    module: &Module,
    vm: &mut Vm,
    exception: ObjectRef,
    from: usize,
    fault_ip: usize,
) -> Result<Status, Trap> {
    let Some(frame) = frames.last() else {
        return Err(Trap::UnhandledException(vm.unhandled_detail(module)));
    };
    let handlers = method_handlers(module, frame.method)?;
    for (index, clause) in handlers.iter().enumerate().skip(from) {
        if !covers(clause.try_range, fault_ip) {
            continue;
        }
        match &clause.kind {
            EhKind::Catch(type_token) => {
                if catch_matches(
                    module,
                    frame.context.asm,
                    vm,
                    *type_token,
                    exception,
                ) {
                    let handler = clause.handler_range.start as usize;
                    let finallys = finallys_inside(&handlers, fault_ip, clause.try_range);
                    abandon_finallys_for(
                        frames.last_mut().ok_or(Trap::StackUnderflow)?,
                        handler,
                    );
                    return begin_finallys(
                        frames,
                        module,
                        vm,
                        finallys,
                        AfterFinally::Catch {
                            handler: clause.handler_range.start as usize,
                            exception,
                        },
                    );
                }
            }
            EhKind::Filter { filter_start } => {
                let pending = PendingFilter {
                    exception,
                    handler: clause.handler_range.start as usize,
                    filter_try: clause.try_range,
                    resume: index + 1,
                    fault_ip,
                };
                let start = *filter_start as usize;
                let frame = frames.last_mut().ok_or(Trap::StackUnderflow)?;
                frame.stack.clear();
                frame.stack.push(Value::Object(exception));
                frame.ip = start;
                frame.pending_filter = Some(pending);
                return Ok(Status::Running);
            }
            EhKind::Finally | EhKind::Fault => {}
        }
    }
    let finallys = finallys_covering(&handlers, fault_ip);
    begin_finallys(
        frames,
        module,
        vm,
        finallys,
        AfterFinally::Unwind(exception),
    )
}

/// Runs the next pending `finally` (if any), else performs the chain's continuation.
fn begin_finallys(
    frames: &mut Vec<Frame>,
    module: &Module,
    vm: &mut Vm,
    mut finallys: Vec<InstructionRange>,
    then: AfterFinally,
) -> Result<Status, Trap> {
    match finallys.pop() {
        Some(next) => {
            let frame = frames.last_mut().ok_or(Trap::StackUnderflow)?;
            frame.ip = next.start as usize;
            frame.pending.push(PendingFinally {
                running: next,
                finallys,
                then,
            });
            Ok(Status::Running)
        }
        None => complete_finally(frames, module, vm, then),
    }
}

/// Ends the frame's pending `finally` chains that an exception leaves by being caught at `handler`:
/// those whose running handler does not hold it.
///
/// A handler that throws is not resumed: an exception that escapes a `finally` abandons the rest of
/// it, and with it the chain that was running it -- a `leave`'s branch, or another exception's
/// unwind, which the new exception replaces as on .NET. The chains nest as their handlers do, so the
/// innermost is ended first and the first that holds `handler` holds every one beneath it. One
/// caught inside the running handler leaves the chain alone, and the handler finishes as written.
/// A chain the exception leaves by unwinding the frame needs nothing: it ends with the frame.
fn abandon_finallys_for(frame: &mut Frame, handler: usize) {
    while frame
        .pending
        .last()
        .is_some_and(|chain| !covers(chain.running, handler))
    {
        frame.pending.pop();
    }
}

/// Performs a finished finally chain's continuation: branch (`leave`), enter the
/// catch, or pop the frame and keep unwinding.
fn complete_finally(
    frames: &mut Vec<Frame>,
    module: &Module,
    vm: &mut Vm,
    then: AfterFinally,
) -> Result<Status, Trap> {
    match then {
        AfterFinally::Goto(target) => {
            frames.last_mut().ok_or(Trap::StackUnderflow)?.ip = target;
            Ok(Status::Running)
        }
        AfterFinally::Catch { handler, exception } => {
            let frame = frames.last_mut().ok_or(Trap::StackUnderflow)?;
            frame.stack.clear();
            frame.stack.push(Value::Object(exception));
            frame.caught_exceptions.retain(|&(start, _)| start != handler);
            frame.caught_exceptions.push((handler, exception));
            frame.ip = handler;
            Ok(Status::Running)
        }
        AfterFinally::Unwind(exception) => {
            #[cfg(feature = "exceptions")]
            let exception = match frames.last() {
                Some(frame) => {
                    let raised = poison_failed_type(module, vm, frame.method, exception);
                    if raised != exception {
                        vm.note_unhandled(raised);
                    }
                    raised
                }
                None => exception,
            };
            if let Some(unwound) = frames.pop() {
                vm.frame_pool.retire(unwound);
            }
            raise(frames, module, vm, exception)
        }
    }
}

/// Whether `ip` lies in the half-open `[start, end)` instruction range.
fn covers(range: InstructionRange, ip: usize) -> bool {
    (range.start as usize) <= ip && ip < (range.end as usize)
}

/// The finally handlers a `leave` from `from_ip` to `target` exits: those whose try
/// covers `from_ip` but not `target`. Ordered so `pop` yields innermost first.
fn finallys_exited(handlers: &[EhClause], from_ip: usize, target: usize) -> Vec<InstructionRange> {
    finally_handlers(handlers, false, &|clause| {
        covers(clause.try_range, from_ip) && !covers(clause.try_range, target)
    })
}

/// The finally handlers in this frame covering `fault_ip` (run as the frame unwinds
/// when it has no matching catch).
fn finallys_covering(handlers: &[EhClause], fault_ip: usize) -> Vec<InstructionRange> {
    finally_handlers(handlers, true, &|clause| covers(clause.try_range, fault_ip))
}

/// The finally handlers nested between `fault_ip` and a catch -- covering the fault
/// and lying within the catch's try region -- run before entering the catch.
fn finallys_inside(
    handlers: &[EhClause],
    fault_ip: usize,
    catch_try: InstructionRange,
) -> Vec<InstructionRange> {
    finally_handlers(handlers, true, &|clause| {
        covers(clause.try_range, fault_ip)
            && clause.try_range.start >= catch_try.start
            && clause.try_range.end <= catch_try.end
    })
}

/// The handler ranges of the finally clauses -- and, when `include_fault` (an exception
/// unwind, not a `leave`), the fault clauses -- kept by `keep`, ordered outermost-first so
/// that `pop` runs them innermost-first. A fault handler runs like a finally during unwind
/// and ends with `endfault` (the same opcode as `endfinally`).
///
/// `keep` is a `dyn` so this is compiled once. Taken as a generic, each of its three callers got
/// a copy of its own, and every copy carried a copy of the sort below.
fn finally_handlers(
    handlers: &[EhClause],
    include_fault: bool,
    keep: &dyn Fn(&EhClause) -> bool,
) -> Vec<InstructionRange> {
    let mut clauses: Vec<&EhClause> = handlers
        .iter()
        .filter(|clause| {
            let runs = matches!(clause.kind, EhKind::Finally)
                || (include_fault && matches!(clause.kind, EhKind::Fault));
            runs && keep(clause)
        })
        .collect();
    // When nested try regions start together, the longer one is outermost.
    clauses.sort_by_key(|clause| {
        (clause.try_range.start, core::cmp::Reverse(clause.try_range.end))
    });
    clauses
        .into_iter()
        .map(|clause| clause.handler_range)
        .collect()
}

/// Whether a `catch` of `type_token` catches `exception` -- a TYPE test: the catch matches
/// only when the thrown exception's runtime type IS-A the catch's declared type. It compares
/// the catch type's exception TAG against the thrown exception's base-chain tag VECTOR
/// (`exception::tag_is_subtype`, the membership the AOT tag contract relies on): a managed
/// exception's vector is its live base chain ([`Module::exception_base_chain`]); a runtime-fault
/// exception's was recorded when the fault was raised ([`Vm::set_exception_chain`]).
///
/// Two cases stay catch-all (matching any in-flight exception), exactly as before: a typeless
/// `catch {}` whose clause carries no catch-type tag, and an exception whose runtime type cannot
/// be identified (no base-chain vector -- e.g. a legacy intrinsic-seam `new Exception()` with no
/// live type), which there is no way to discriminate by type.
fn catch_matches(
    module: &Module,
    asm: u8,
    vm: &Vm,
    type_token: Token,
    exception: ObjectRef,
) -> bool {
    #[cfg(feature = "exceptions")]
    {
        let Some(catch_tag) = module.catch_type_tag(asm, type_token) else {
            return true;
        };
        if crate::exception::is_universal_catch(catch_tag) {
            return true;
        }
        let managed_chain;
        let thrown_chain: &[u32] = match vm.heap().type_of(exception) {
            Some(EXTERNAL_TYPE_ID) | None => vm.exception_chain(exception).unwrap_or(&[]),
            Some(type_id) => {
                managed_chain = module.exception_base_chain(type_id);
                &managed_chain
            }
        };
        if thrown_chain.is_empty() {
            return true;
        }
        crate::exception::tag_is_subtype(catch_tag, thrown_chain)
    }
    #[cfg(not(feature = "exceptions"))]
    {
        let _ = (module, asm, vm, type_token, exception);
        true
    }
}

/// The out-of-memory guard an allocation opcode (`newobj`, `newarr`, `box`) runs before it
/// grows the heap: with a hard object budget configured (an embedder on a fixed heap sets
/// one below the real capacity), refuse the allocation once the live object count has reached
/// it, returning `Trap::OutOfMemory`. The caller (`advance`) turns that trap into a CATCHABLE
/// `OutOfMemoryException` via [`catchable_fault`], so `catch (OutOfMemoryException)` runs and a
/// handler that drops references lets the next collection recover -- exactly the allocation
/// site .NET throws OOM at, not an instruction-boundary poll (which would re-throw before the
/// handler's own first instruction could run). The safepoint at the top of `advance` has
/// already collected on this pressure (its GC condition includes `at_budget`), so this reads
/// the POST-collection count and fires only when a collection could not free the room. The
/// exception object itself allocates through [`Heap::alloc`] directly, using the headroom the
/// budget deliberately leaves below capacity. Unbounded by default (`object_budget = None`),
/// so a host that sets no budget never pays for the check.
#[inline]
pub(crate) fn check_alloc_budget(vm: &Vm) -> Result<(), Trap> {
    if vm.heap().at_budget() {
        return Err(Trap::OutOfMemory);
    }
    Ok(())
}

/// Allocate a zero-initialized array of `length` elements whose element type is `token` in
/// assembly `asm`, and answer the new object.
///
/// This is the WHOLE of what `newarr` (III.4.20) does after its operand and length are in hand,
/// and it is a function rather than an inline arm because `System.Array.CreateInstance` asks the
/// identical question at run time from a `Type` handle. Two implementations of "what is this
/// element type's zero, and does it pack" would be two answers to a question whose wrong answer is
/// silent -- an array of nulls where .NET has zeroed structs reads as working code until an
/// element is used.
///
/// The element type's kind is the one the loader bound to `token`, an enum's underlying integer's, or,
/// where neither applies, that of the built-in value type `token` names ([`builtin_storage`]): a
/// generic body's `new T[n]` at `T = int` names `System.Int32` through the token its `T` was
/// rewritten to, and the array it makes is an `int[]` (ECMA-335 6th ed III.4.20), packed as a written
/// `new int[n]` is. A native int's elements are the executing target's pointer width (I.12.1.1).
pub(crate) fn alloc_array_of_element(
    vm: &mut Vm,
    module: &Module,
    asm: u8,
    token: Token,
    length: usize,
) -> ObjectRef {
    use crate::module::FieldStorage;
    let element_type = asm_key(asm, token.0);
    // An enum's storage is its underlying integer's (ECMA-335 6th ed II.14.3), so its array packs as
    // that integer's does, and a pointer into it reads and writes those bytes.
    let kind = module
        .array_prim_kind(asm, token)
        .or_else(|| module.enum_underlying_prim_by_handle(element_type).and_then(CastPrim::prim_kind));
    let builtin = match kind {
        Some(kind) => Some(FieldStorage::Primitive(kind)),
        None => module.type_id_of(asm, token).and_then(|id| builtin_storage(module, id)),
    };
    match builtin {
        Some(FieldStorage::Primitive(kind)) if kind.packable() => {
            vm.heap_mut().alloc_packed_array(kind, length, element_type)
        }
        _ => {
            let default = match module.array_default(asm, token) {
                Some(default) => default,
                None if !module.is_enum_by_handle(element_type) => module
                    .type_id_of(asm, token)
                    .filter(|&type_id| module.type_is_value_type(type_id))
                    .and_then(|type_id| module.type_field_defaults(type_id))
                    .map_or(Value::Null, |fields| Value::Struct(fields.into_boxed_slice())),
                None => Value::Int32(0),
            };
            // Preserve the declared storage size independently of the reduced value slots.
            // A reference to a type in another assembly uses that type's defining handle.
            let element_width = if matches!(builtin, Some(FieldStorage::Native)) {
                Some(crate::native_pointer_size() as usize)
            } else {
                module.type_size(asm, token).or_else(|| {
                    let canonical = module.canonical_type_handle(element_type)?;
                    module.type_size((canonical >> 32) as u8, Token(canonical as u32))
                }).or_else(|| {
                    let type_id = module.type_id_of(asm, token)?;
                    module.type_is_value_type(type_id).then_some(type_id)
                        .and_then(|id| module.type_field_offsets(id)).map(|layout| layout.end())
                }).map(|size| size as usize)
            };
            vm.heap_mut().alloc_array_with_element_width(
                alloc::vec![default; length], element_type, element_width,
            )
        }
    }
}

/// Allocates the rectangular array the constructor `token` in assembly `asm` creates, with the
/// lengths `dims`. Its elements are of its element type (ECMA-335 6th ed I.8.9.1) and start at that
/// type's zero (II.13.2): a sized primitive or an enum (its underlying integer, II.14.3) is packed as
/// a vector's elements are; an unmanaged pointer or a native int starts as the native int 0, the null
/// pointer (I.12.1.1.1); a struct as its zero instance; a reference as null. An element type the
/// loader recorded no shape for, or a generic value type, starts as the int32 0.
fn alloc_md_array_of_element(vm: &mut Vm, module: &Module, asm: u8, token: Token, dims: Vec<i32>) -> ObjectRef {
    let array_type = asm_key(asm, token.0);
    let element = match module.cast_elem(array_type) {
        Some(CastElem::MdArray { element, .. }) => Some(*element),
        _ => None,
    };
    let kind = match &element {
        Some(CastElem::Prim(prim)) => prim.prim_kind(),
        Some(CastElem::Named(handle)) => module.enum_underlying_prim_by_handle(*handle).and_then(CastPrim::prim_kind),
        _ => None,
    };
    if let Some(kind) = kind.filter(|kind| kind.packable()) {
        return vm.heap_mut().alloc_md_array_packed(dims, kind, array_type);
    }
    let zero = match element {
        Some(CastElem::Prim(CastPrim::I | CastPrim::U) | CastElem::Pointer(_)) => Value::NativeInt(0),
        Some(CastElem::Named(handle)) if kind.is_none() => match module.type_id_by_handle(handle) {
            Some(id) if module.type_is_value_type(id) => module
                .type_field_defaults(id)
                .map_or(Value::Int32(0), |fields| Value::Struct(fields.into_boxed_slice())),
            Some(_) => Value::Null,
            None => Value::Int32(0),
        },
        Some(
            CastElem::String
            | CastElem::Object
            | CastElem::Array(_)
            | CastElem::MdArray { .. }
            | CastElem::VectorInterface(_)
            | CastElem::Lenient,
        ) => Value::Null,
        _ => Value::Int32(0),
    };
    vm.heap_mut().alloc_md_array_of(dims, zero, array_type)
}

fn step(
    frame: &mut Frame,
    frame_index: usize,
    callers: &[Frame],
    code_len: usize,
    module: Option<&Module>,
    vm: &mut Vm,
    instruction: &Instruction,
) -> Result<Flow, Trap> {
    let opcode = instruction.opcode;
    let readonly = if matches!(opcode,
        Opcode::Readonly | Opcode::Constrained | Opcode::Volatile | Opcode::Unaligned | Opcode::Tail
    ) {
        false
    } else {
        core::mem::take(&mut frame.pending_readonly)
    };
    let asm = frame.context.asm;
    match opcode {
        Opcode::Nop => {}
        Opcode::Break => {}
        Opcode::Pop => {
            frame.pop()?;
        }
        Opcode::Dup => {
            let top = frame.stack.last().ok_or(Trap::StackUnderflow)?.clone();
            frame.stack.push(top);
        }

        Opcode::LdcI4M1 => frame.stack.push(Value::Int32(-1)),
        Opcode::LdcI40 => frame.stack.push(Value::Int32(0)),
        Opcode::LdcI41 => frame.stack.push(Value::Int32(1)),
        Opcode::LdcI42 => frame.stack.push(Value::Int32(2)),
        Opcode::LdcI43 => frame.stack.push(Value::Int32(3)),
        Opcode::LdcI44 => frame.stack.push(Value::Int32(4)),
        Opcode::LdcI45 => frame.stack.push(Value::Int32(5)),
        Opcode::LdcI46 => frame.stack.push(Value::Int32(6)),
        Opcode::LdcI47 => frame.stack.push(Value::Int32(7)),
        Opcode::LdcI48 => frame.stack.push(Value::Int32(8)),
        Opcode::LdcI4S => frame
            .stack
            .push(Value::Int32(int8_operand(instruction)? as i32)),
        Opcode::LdcI4 => frame.stack.push(Value::Int32(int32_operand(instruction)?)),
        Opcode::LdcI8 => frame.stack.push(Value::Int64(int64_operand(instruction)?)),
        #[cfg(feature = "float")]
        Opcode::LdcR4 => frame
            .stack
            .push(Value::Single(float32_operand(instruction)?)),
        #[cfg(feature = "float")]
        Opcode::LdcR8 => frame
            .stack
            .push(Value::Float(float64_operand(instruction)?)),
        Opcode::Ldnull => frame.stack.push(Value::Null),

        Opcode::Ldarg0 => frame.load_arg(0)?,
        Opcode::Ldarg1 => frame.load_arg(1)?,
        Opcode::Ldarg2 => frame.load_arg(2)?,
        Opcode::Ldarg3 => frame.load_arg(3)?,
        Opcode::LdargS | Opcode::Ldarg => frame.load_arg(var_operand(instruction)?)?,
        Opcode::Ldloc0 => frame.load_local(0),
        Opcode::Ldloc1 => frame.load_local(1),
        Opcode::Ldloc2 => frame.load_local(2),
        Opcode::Ldloc3 => frame.load_local(3),
        Opcode::LdlocS | Opcode::Ldloc => frame.load_local(var_operand(instruction)?),
        Opcode::Stloc0 | Opcode::Stloc1 | Opcode::Stloc2 | Opcode::Stloc3 | Opcode::StlocS | Opcode::Stloc => {
            let slot = match opcode {
                Opcode::Stloc0 => 0,
                Opcode::Stloc1 => 1,
                Opcode::Stloc2 => 2,
                Opcode::Stloc3 => 3,
                _ => var_operand(instruction)?,
            };
            if let Some(module) = module {
                declare_top(frame, |method| module.method_local_kind(method, usize::from(slot)));
            }
            frame.store_local(slot)?;
        }
        Opcode::StargS | Opcode::Starg => {
            let slot = var_operand(instruction)?;
            if let Some(module) = module {
                declare_top(frame, |method| module.method_arg_kind(method, usize::from(slot)));
            }
            frame.store_arg(slot)?;
        }
        Opcode::LdlocaS | Opcode::Ldloca => frame.stack.push(Value::ByRef(Location::Local {
            frame: frame_index,
            slot: var_operand(instruction)? as usize,
        })),
        Opcode::LdargaS | Opcode::Ldarga => frame.stack.push(Value::ByRef(Location::Arg {
            frame: frame_index,
            slot: var_operand(instruction)? as usize,
        })),

        Opcode::Add
        | Opcode::Sub
        | Opcode::Mul
        | Opcode::Div
        | Opcode::Rem
        | Opcode::AddOvf
        | Opcode::AddOvfUn
        | Opcode::SubOvf
        | Opcode::SubOvfUn
        | Opcode::MulOvf
        | Opcode::MulOvfUn => {
            let (a, b) = frame.pop2()?;
            let (a, b) = variable_bytes_pair(module, frame.method, frame_index, callers, a, b);
            let result = match stack_pointer_arithmetic(vm.heap(), opcode, &a, &b)? {
                Some(pointer) => pointer,
                None if matches!(a, Value::ByRef(_)) || matches!(b, Value::ByRef(_)) => {
                    match field_pointer_arithmetic(module, vm, opcode, &a, &b)? {
                        Some(pointer) => pointer,
                        None => binary_numeric(opcode, a, b)?,
                    }
                }
                None => binary_numeric(opcode, a, b)?,
            };
            frame.stack.push(result);
        }
        Opcode::DivUn | Opcode::RemUn | Opcode::And | Opcode::Or | Opcode::Xor => {
            let (a, b) = frame.pop2()?;
            frame.stack.push(binary_integer(opcode, a, b)?);
        }
        Opcode::Shl | Opcode::Shr | Opcode::ShrUn => {
            let (value, amount) = frame.pop2()?;
            frame.stack.push(shift(opcode, value, amount)?);
        }
        Opcode::Neg => {
            let value = frame.pop()?;
            frame.stack.push(negate(value)?);
        }
        Opcode::Not => {
            let value = frame.pop()?;
            frame.stack.push(bitwise_not(value)?);
        }
        #[cfg(feature = "float")]
        Opcode::Ckfinite => {
            let value = frame.pop()?;
            match value {
                Value::Float(x) if x.is_finite() => frame.stack.push(value),
                Value::Single(x) if x.is_finite() => frame.stack.push(value),
                Value::Float(_) | Value::Single(_) => return Err(Trap::Overflow),
                _ => return Err(Trap::TypeMismatch(Opcode::Ckfinite)),
            }
        }

        Opcode::ConvI1
        | Opcode::ConvI2
        | Opcode::ConvI4
        | Opcode::ConvI8
        | Opcode::ConvU1
        | Opcode::ConvU2
        | Opcode::ConvU4
        | Opcode::ConvU8
        | Opcode::ConvI
        | Opcode::ConvU
        | Opcode::ConvR4
        | Opcode::ConvR8
        | Opcode::ConvRUn => {
            let value = frame.pop()?;
            let converted = match value {
                // A pinned string's logical native address names its first UTF-16 unit.
                // RuntimeHelpers.OffsetToStringData uses the same headerless address space.
                Value::Object(string)
                    if matches!(opcode, Opcode::ConvI | Opcode::ConvU)
                        && vm.heap().is_string(string) =>
                {
                    Value::ByRef(Location::StringChar { string, byte_offset: 0 })
                }
                Value::Null if matches!(opcode, Opcode::ConvI | Opcode::ConvU) => Value::NativeInt(0),
                Value::ByRef(_)
                    if matches!(
                        opcode,
                        Opcode::ConvU | Opcode::ConvI | Opcode::ConvU8 | Opcode::ConvI8
                    ) =>
                {
                    value
                }
                other => convert(opcode, other)?,
            };
            frame.stack.push(converted);
        }
        Opcode::ConvOvfI1
        | Opcode::ConvOvfI1Un
        | Opcode::ConvOvfU1
        | Opcode::ConvOvfU1Un
        | Opcode::ConvOvfI2
        | Opcode::ConvOvfI2Un
        | Opcode::ConvOvfU2
        | Opcode::ConvOvfU2Un
        | Opcode::ConvOvfI4
        | Opcode::ConvOvfI4Un
        | Opcode::ConvOvfU4
        | Opcode::ConvOvfU4Un
        | Opcode::ConvOvfI8
        | Opcode::ConvOvfI8Un
        | Opcode::ConvOvfU8
        | Opcode::ConvOvfU8Un
        | Opcode::ConvOvfI
        | Opcode::ConvOvfIUn
        | Opcode::ConvOvfU
        | Opcode::ConvOvfUUn => {
            let value = frame.pop()?;
            frame.stack.push(convert_checked(opcode, value)?);
        }

        Opcode::Ceq | Opcode::Cgt | Opcode::CgtUn | Opcode::Clt | Opcode::CltUn => {
            let (a, b) = frame.pop2()?;
            let (a, b) = variable_bytes_pair(module, frame.method, frame_index, callers, a, b);
            let result = compare(vm.heap(), &field_offset_in(module, vm, frame.method, frame_index, callers), opcode, a, b)?;
            frame.stack.push(Value::Int32(i32::from(result)));
        }

        Opcode::Br | Opcode::BrS => frame.ip = branch_target(instruction, code_len)?,
        Opcode::Brtrue | Opcode::BrtrueS => {
            let value = frame.pop()?;
            if value.is_truthy() {
                frame.ip = branch_target(instruction, code_len)?;
            }
        }
        Opcode::Brfalse | Opcode::BrfalseS => {
            let value = frame.pop()?;
            if !value.is_truthy() {
                frame.ip = branch_target(instruction, code_len)?;
            }
        }
        Opcode::Beq
        | Opcode::BeqS
        | Opcode::BneUn
        | Opcode::BneUnS
        | Opcode::Bge
        | Opcode::BgeS
        | Opcode::BgeUn
        | Opcode::BgeUnS
        | Opcode::Bgt
        | Opcode::BgtS
        | Opcode::BgtUn
        | Opcode::BgtUnS
        | Opcode::Ble
        | Opcode::BleS
        | Opcode::BleUn
        | Opcode::BleUnS
        | Opcode::Blt
        | Opcode::BltS
        | Opcode::BltUn
        | Opcode::BltUnS => {
            let (a, b) = frame.pop2()?;
            let (a, b) = variable_bytes_pair(module, frame.method, frame_index, callers, a, b);
            if compare(vm.heap(), &field_offset_in(module, vm, frame.method, frame_index, callers), opcode, a, b)? {
                frame.ip = branch_target(instruction, code_len)?;
            }
        }
        Opcode::Switch => {
            let index = unsigned_index(frame.pop()?).ok_or(Trap::TypeMismatch(Opcode::Switch))?;
            let Operand::Switch(targets) = &instruction.operand else {
                return Err(Trap::MalformedInstruction(Opcode::Switch));
            };
            if let Some(&target) = targets.get(index) {
                if target as usize >= code_len {
                    return Err(Trap::BranchOutOfRange(target));
                }
                frame.ip = target as usize;
            }
        }

        Opcode::Ldstr => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldstr))?;
            let token = token_operand(instruction)?;
            let chars = module
                .resolve_string_le(asm, token)
                .ok_or(Trap::UnresolvedString(token))?;
            let reference = vm.heap_mut().intern_string_le(chars)?;
            frame.stack.push(Value::Object(reference));
        }

        Opcode::Call => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Call))?;
            let token = token_operand(instruction)?;
            let Some(method) = module.resolve(asm, token) else {
                if let Some(target) = module.pinvoke_target(asm, token.0) {
                    return pinvoke_call(frame, vm, target, token);
                }
                return Err(unresolved_call(module, asm, token));
            };
            if let Some(site) = module.vararg_site(asm, token) {
                let total = site.total_args;
                let vararg_start = site.vararg_start;
                let var_types = site.var_types.clone();
                let args = take_args_pooled(frame, vm, total)?;
                return Ok(Flow::CallVararg {
                    method,
                    args,
                    vararg_start,
                    var_types,
                });
            }
            let arg_count = module
                .method_arg_count(method)
                .ok_or(Trap::NoSuchMethod(method))?;
            let args = take_args_pooled(frame, vm, arg_count)?;
            return Ok(Flow::Call { method, args });
        }

        Opcode::Callvirt => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Callvirt))?;
            let token = token_operand(instruction)?;
            if let Some(param_count) = module.delegate_invoke(asm, token) {
                let args = take_args_pooled(frame, vm, param_count + 1)?;
                let delegate = object_ref(
                    args.first().ok_or(Trap::StackUnderflow)?.clone(),
                    Opcode::Callvirt,
                )?;
                let invocations = vm
                    .heap()
                    .delegate_invocations(delegate)
                    .ok_or(Trap::TypeMismatch(Opcode::Callvirt))?
                    .to_vec();
                let params = args.get(1..).unwrap_or_default().to_vec();
                vm.frame_pool.give_values(args);
                return match invocations.split_first() {
                    Some(((target, method), [])) => Ok(Flow::Call {
                        method: *method,
                        args: delegate_receiver_args(module, vm, *method, target, &params),
                    }),
                    Some(_) => Ok(Flow::InvokeMulticast {
                        invocations,
                        params,
                    }),
                    None => Err(Trap::TypeMismatch(Opcode::Callvirt)),
                };
            }
            let static_method = module.resolve(asm, token);
            let target_info = module.call_target(asm, token);
            let arg_count = match target_info {
                Some((_, count)) => count,
                None => {
                    let method =
                        static_method.ok_or_else(|| unresolved_call(module, asm, token))?;
                    module
                        .method_arg_count(method)
                        .ok_or(Trap::NoSuchMethod(method))?
                }
            };
            let mut args = take_args_pooled(frame, vm, arg_count)?;
            let sig_key = target_info.map(|(key, _)| key);
            let constraint = frame.pending_constraint.take();
            // ECMA-335 III.2.1: a reference-type constraint dereferences `this` before
            // callvirt, including its null check and dispatch on the object's runtime type.
            // Only value-type constraints keep the managed pointer and constraint dispatch.
            let constraint = match constraint {
                Some(token) if is_value_type_operand(module, asm, token) == Some(false) => {
                    let receiver = args.first_mut().ok_or(Trap::StackUnderflow)?;
                    let Value::ByRef(location) = receiver else {
                        return Err(Trap::TypeMismatch(Opcode::Callvirt));
                    };
                    *receiver = read_constrained_receiver(frame, frame_index, callers, vm, location)
                        .ok_or(Trap::Unsupported(Opcode::Callvirt))?;
                    None
                }
                other => other,
            };
            let runtime_type = match constraint {
                Some(constraint) => module.type_id_of(asm, constraint),
                None => match args.first().ok_or(Trap::StackUnderflow)? {
                    Value::Object(this) => receiver_type_id(module, vm, *this),
                    Value::Null => return Err(Trap::NullReference),
                    _ => None,
                },
            };
            let explicit_override =
                runtime_type.and_then(|type_id| module.explicit_override(asm, type_id, token));
            let method =
                resolve_callvirt(module, static_method, sig_key, runtime_type, explicit_override)
                    .ok_or_else(|| unresolved_call(module, asm, token))?;
            #[cfg(feature = "bcl")]
            if let Some(constraint) = constraint {
                if is_object_to_string(module, method) {
                    let handle = asm_key(asm, constraint.0);
                    let enum_value = match args.first() {
                        Some(Value::ByRef(location)) => {
                            refuse_cast_receiver(frame, frame_index, callers, vm, module, location, asm, constraint)?;
                            read_enum_value(frame, frame_index, callers, vm, location.clone())
                        }
                        _ => None,
                    };
                    let unread = enum_value.is_none() && matches!(args.first(), Some(Value::ByRef(_)));
                    let declared = module
                        .type_id_of(asm, constraint)
                        .and_then(|id| module.type_handle_of(id))
                        .and_then(|handle| module.enum_underlying_prim_by_handle(handle));
                    let enum_value = enum_value.map(|value| declared.map_or(value, |cast| with_declared_sign(cast, value)));
                    if let Some(value) = enum_value {
                        if let Some(name) = module.enum_name_or_flags(handle, value, false) {
                            let string = vm.heap_mut().alloc_text(&name);
                            frame.stack.push(Value::Object(string));
                            return Ok(Flow::Next);
                        }
                        if module.is_enum_by_handle(handle) {
                            let string = vm.heap_mut().alloc_text(&value.to_string());
                            frame.stack.push(Value::Object(string));
                            return Ok(Flow::Next);
                        }
                    }
                    if unread && module.is_enum_by_handle(handle) {
                        return Err(Trap::Unsupported(Opcode::Callvirt));
                    }
                    if let Some(name) = module.type_name_by_handle(handle) {
                        let string = vm.heap_mut().alloc_text(name);
                        frame.stack.push(Value::Object(string));
                        return Ok(Flow::Next);
                    }
                }
            }
            if let Some(constraint) = constraint {
                if !module.method_declares_value_type(method) {
                    if let Some(Value::ByRef(location)) = args.first().cloned() {
                        refuse_cast_receiver(frame, frame_index, callers, vm, module, &location, asm, constraint)?;
                        match read_constrained_receiver(frame, frame_index, callers, vm, &location) {
                            None => {}
                            Some(value @ (Value::Object(_) | Value::Null)) => args[0] = value,
                            Some(value)
                                if runtime_type
                                    .is_some_and(|type_id| !module.type_is_value_type(type_id)) =>
                            {
                                args[0] = value;
                            }
                            Some(value) => {
                                check_alloc_budget(vm)?;
                                let boxed =
                                    vm.heap_mut().alloc_boxed(asm_key(asm, constraint.0), value);
                                args[0] = Value::Object(boxed);
                            }
                        }
                    }
                }
            }
            value_type_receiver(module, vm, method, &mut args);
            return Ok(Flow::Call { method, args });
        }

        Opcode::Calli => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Calli))?;
            let pointer = function_pointer(frame.pop()?)?;
            let arg_count = module
                .method_arg_count(pointer)
                .ok_or(Trap::NoSuchMethod(pointer))?;
            let args = take_args_pooled(frame, vm, arg_count)?;
            return Ok(Flow::Call {
                method: pointer,
                args,
            });
        }
        Opcode::Jmp => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Jmp))?;
            let token = token_operand(instruction)?;
            let target = module
                .resolve(asm, token)
                .ok_or_else(|| unresolved_call(module, asm, token))?;
            return Ok(Flow::Jmp(target));
        }
        Opcode::Ldftn => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldftn))?;
            let token = token_operand(instruction)?;
            match module.resolve(asm, token) {
                Some(method) => frame.stack.push(Value::NativeInt(i64::from(method))),
                None if module.delegate_invoke(asm, token).is_some() => {
                    frame.stack.push(Value::NativeInt(i64::from(DELEGATE_INVOKE_FPTR)));
                }
                None => return Err(unresolved_call(module, asm, token)),
            }
        }
        Opcode::Ldvirtftn => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldvirtftn))?;
            let token = token_operand(instruction)?;
            let this = object_ref(frame.pop()?, Opcode::Ldvirtftn)?;
            let runtime_type = receiver_type_id(module, vm, this);
            let sig_key = module.call_target(asm, token).map(|(key, _)| key);
            let explicit_override =
                runtime_type.and_then(|type_id| module.explicit_override(asm, type_id, token));
            let resolved = resolve_callvirt(
                module,
                module.resolve(asm, token),
                sig_key,
                runtime_type,
                explicit_override,
            );
            match resolved {
                Some(method) => frame.stack.push(Value::NativeInt(i64::from(method))),
                None if module.delegate_invoke(asm, token).is_some() => {
                    frame.stack.push(Value::NativeInt(i64::from(DELEGATE_INVOKE_FPTR)));
                }
                None => return Err(unresolved_call(module, asm, token)),
            }
        }

        Opcode::Newobj => {
            check_alloc_budget(vm)?;
            let module = module.ok_or(Trap::Unsupported(Opcode::Newobj))?;
            let token = token_operand(instruction)?;
            if module.is_delegate_ctor(asm, token) {
                let method = function_pointer(frame.pop()?)?;
                let target = frame.pop()?;
                let delegate_type = module.delegate_ctor_type(asm, token);
                let delegate = if method == DELEGATE_INVOKE_FPTR {
                    let operand = object_ref(target, Opcode::Newobj)?;
                    let invocations = vm
                        .heap()
                        .delegate_invocations(operand)
                        .ok_or(Trap::TypeMismatch(Opcode::Newobj))?
                        .to_vec();
                    vm.heap_mut().alloc_multicast(invocations, delegate_type)
                } else {
                    vm.heap_mut().alloc_delegate(target, method, delegate_type)
                };
                frame.stack.push(Value::Object(delegate));
                return Ok(Flow::Next);
            }
            if let Some(rank) = module.md_array_ctor_rank(asm, token) {
                let lengths = take_args_pooled(frame, vm, rank)?;
                let dims: Vec<i32> = lengths
                    .iter()
                    .map(|value| match value {
                        Value::Int32(n) => *n,
                        Value::Int64(n) | Value::NativeInt(n) => *n as i32,
                        _ => 0,
                    })
                    .collect();
                vm.frame_pool.give_values(lengths);
                let array = alloc_md_array_of_element(vm, module, asm, token, dims);
                frame.stack.push(Value::Object(array));
                return Ok(Flow::Next);
            }
            if let Some(params) = module.string_builder_ctor_params(asm, token) {
                const DEFAULT_CAPACITY: usize = 16;
                let args = take_args_pooled(frame, vm, params)?;
                let (initial, capacity) = match args.first() {
                    Some(&Value::Object(reference)) => {
                        let units = vm
                            .heap()
                            .as_string(reference)
                            .map(|units| units.into_owned())
                            .unwrap_or_default();
                        (units, DEFAULT_CAPACITY)
                    }
                    Some(&Value::Int32(requested)) => {
                        let capacity = if requested <= 0 {
                            DEFAULT_CAPACITY
                        } else {
                            requested as usize
                        };
                        (Vec::new(), capacity)
                    }
                    _ => (Vec::new(), DEFAULT_CAPACITY),
                };
                vm.frame_pool.give_values(args);
                let builder = vm.heap_mut().alloc_string_builder(initial, capacity);
                frame.stack.push(Value::Object(builder));
                return Ok(Flow::Next);
            }
            if let Some(params) = module.list_ctor_params(asm, token) {
                let split = frame
                    .stack
                    .len()
                    .checked_sub(params as usize)
                    .ok_or(Trap::StackUnderflow)?;
                frame.stack.truncate(split);
                let list = vm.heap_mut().alloc_array(Vec::new());
                frame.stack.push(Value::Object(list));
                return Ok(Flow::Next);
            }
            let ctor = module
                .resolve(asm, token)
                .ok_or_else(|| unresolved_call(module, asm, token))?;
            if let Some(with_range) = string_pointer_ctor_form(module, ctor) {
                let mut args = take_args_pooled(frame, vm, if with_range { 3 } else { 1 })?;
                let (start, length) = if with_range {
                    (ctor_int_arg(&args, 1)?, Some(ctor_int_arg(&args, 2)?))
                } else {
                    (0, None)
                };
                let pointer = args.swap_remove(0);
                vm.frame_pool.give_values(args);
                return Ok(Flow::NewString {
                    pointer,
                    start,
                    length,
                });
            }
            #[cfg(feature = "bcl")]
            if let Some(form) = string_value_ctor_form(module, ctor) {
                let arity = match form {
                    StringValueCtor::WholeArray => 1,
                    StringValueCtor::ArrayRange => 3,
                    StringValueCtor::Repeat => 2,
                };
                let args = take_args_pooled(frame, vm, arity)?;
                let built = build_string_value(vm, &args, form);
                vm.frame_pool.give_values(args);
                frame.stack.push(Value::Object(built?));
                return Ok(Flow::Next);
            }
            let is_intrinsic =
                matches!(module.method_kind(ctor), Some(MethodKind::Intrinsic(_)));
            let (type_id, defaults) = if is_intrinsic {
                (EXTERNAL_TYPE_ID, Vec::new())
            } else {
                let type_id = module.method_type(ctor).ok_or(Trap::NoSuchMethod(ctor))?;
                let defaults = module
                    .type_field_defaults(type_id)
                    .ok_or(Trap::NoSuchMethod(ctor))?;
                (type_id, defaults)
            };
            let param_count = module
                .method_arg_count(ctor)
                .ok_or(Trap::NoSuchMethod(ctor))?
                .saturating_sub(1);
            let args = take_args_pooled(frame, vm, param_count)?;
            if module.is_value_type_ctor(asm, token) {
                let zero = Value::Struct(defaults.into_boxed_slice());
                let temporary = vm.heap_mut().alloc_boxed(asm_key(asm, token.0), zero);
                return Ok(Flow::NewValueObj {
                    ctor,
                    location: Location::Boxed { object: temporary },
                    args,
                });
            }
            let object = vm.heap_mut().alloc_instance(type_id, defaults);
            #[cfg(feature = "finalizers")]
            if module.finalizer_of(type_id).is_some() {
                vm.heap_mut().register_finalizer(object);
            }
            return Ok(Flow::NewObj { ctor, object, args });
        }

        Opcode::Ldfld => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldfld))?;
            let token = token_operand(instruction)?;
            let slot = module
                .field_slot(asm, token)
                .ok_or_else(|| unresolved_field(module, asm, token))?;
            match frame.pop()? {
                Value::Object(object) => {
                    let value = vm
                        .heap()
                        .instance_field(object, slot)
                        .ok_or(Trap::FieldSlotMissing { field: token, slot })?;
                    frame.stack.push(value);
                }
                Value::ByRef(location) => {
                    return Ok(Flow::LoadField {
                        location,
                        field: token,
                    });
                }
                Value::Struct(fields) => {
                    let value = fields
                        .get(slot as usize)
                        .cloned()
                        .ok_or(Trap::FieldSlotMissing { field: token, slot })?;
                    frame.stack.push(value);
                }
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Ldfld)),
            }
        }
        Opcode::Stfld => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Stfld))?;
            let token = token_operand(instruction)?;
            let slot = module
                .field_slot(asm, token)
                .ok_or_else(|| unresolved_field(module, asm, token))?;
            let value = frame.pop()?;
            let value = if depends_on_slot_kind(&value) {
                let storage = module.field_type(asm, token).and_then(|owner| module.type_field_storage(owner, slot as usize));
                stored_at_storage(storage, value)
            } else {
                value
            };
            match frame.pop()? {
                Value::Object(object) => {
                    if !vm.heap_mut().set_instance_field(object, slot, value) {
                        return Err(Trap::FieldSlotMissing { field: token, slot });
                    }
                }
                Value::ByRef(location) => {
                    return Ok(Flow::StoreField {
                        location,
                        field: token,
                        value,
                    });
                }
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Stfld)),
            }
        }
        Opcode::Ldflda => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldflda))?;
            let token = token_operand(instruction)?;
            let slot = module
                .field_slot(asm, token)
                .ok_or_else(|| unresolved_field(module, asm, token))?;
            match frame.pop()? {
                Value::Object(object) => {
                    frame
                        .stack
                        .push(Value::ByRef(Location::Field { object, slot }));
                }
                Value::Null => return Err(Trap::NullReference),
                Value::ByRef(base @ Location::Stack { .. }) => {
                    let (address, _) = stack_field_address(module, asm, token, slot, base, Opcode::Ldflda)?;
                    frame.stack.push(Value::ByRef(address));
                }
                Value::ByRef(base) => {
                    frame.stack.push(Value::ByRef(Location::Nested {
                        base: alloc::boxed::Box::new(base),
                        slot,
                    }));
                }
                _ => return Err(Trap::Unsupported(Opcode::Ldflda)),
            }
        }
        Opcode::Ldsflda => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldsflda))?;
            let token = token_operand(instruction)?;
            let slot = module
                .static_field_slot(asm, token)
                .ok_or_else(|| unresolved_field(module, asm, token))?;
            if let Some(type_id) = module.type_of_static_slot(slot) {
                if let Some(cctor) = vm.take_pending_cctor(module, type_id)
                    .map_err(Trap::TypeInitializationFailed)? {
                    return Ok(Flow::EnsureCctor(cctor));
                }
            }
            if vm.statics_len() < module.static_field_count() {
                vm.init_statics(&module.decode_static_defaults());
            }
            frame.stack.push(Value::ByRef(Location::Static { slot }));
        }
        Opcode::Ldelema => {
            let index = array_index(frame.pop()?, Opcode::Ldelema)?;
            let array = object_ref(frame.pop()?, Opcode::Ldelema)?;
            let len = vm
                .heap()
                .array_len(array)
                .ok_or(Trap::TypeMismatch(Opcode::Ldelema))?;
            let index = bounded_index(index, len)?;
            if !readonly {
                if let (Some(module), Some(element)) = (module, vm.heap().array_element_type(array)) {
                    let token = token_operand(instruction)?;
                    if element != 0 && !array_element_is(module, element, asm, token) {
                        let held = classify_elem_handle(module, element);
                        let wanted = classify_elem_handle(module, asm_key(asm, token.0));
                        if !elem_compatible(module, &held, &wanted, ElemRule::Address) {
                            return Err(Trap::ArrayTypeMismatch);
                        }
                    }
                }
            }
            frame.stack.push(Value::ByRef(Location::Element {
                array,
                index,
                byte_offset: 0,
            }));
        }
        Opcode::Initobj => {
            let token = token_operand(instruction)?;
            match frame.pop()? {
                Value::ByRef(location) => {
                    return Ok(Flow::InitObj {
                        location,
                        kind: token,
                    });
                }
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Initobj)),
            }
        }
        Opcode::Ldobj => match frame.pop()? {
            Value::ByRef(location) => return Ok(Flow::LoadObj { location, opcode, token: token_operand(instruction)? }),
            other @ (Value::Struct(_) | Value::Object(_)) => frame.stack.push(other),
            Value::Null => return Err(Trap::NullReference),
            _ => return Err(Trap::TypeMismatch(Opcode::Ldobj)),
        },
        Opcode::Stobj => {
            let value = frame.pop()?;
            match frame.pop()? {
                Value::ByRef(location) => return Ok(Flow::StoreObj { location, value, opcode, token: token_operand(instruction)? }),
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Stobj)),
            }
        }
        Opcode::Cpobj => {
            let src = match frame.pop()? {
                Value::ByRef(location) => location,
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Cpobj)),
            };
            match frame.pop()? {
                Value::ByRef(dest) => return Ok(Flow::CopyObj { dest, src }),
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Cpobj)),
            }
        }
        Opcode::Constrained => {
            frame.pending_constraint = Some(token_operand(instruction)?);
        }
        // A readonly element address can safely read through a covariant array reference.
        // Writable addresses require an exact reference element type before exposing storage.
        Opcode::Readonly => frame.pending_readonly = true,
        Opcode::Volatile | Opcode::Unaligned | Opcode::Tail => {}
        Opcode::LdindI1
        | Opcode::LdindU1
        | Opcode::LdindI2
        | Opcode::LdindU2
        | Opcode::LdindI4
        | Opcode::LdindU4
        | Opcode::LdindI8
        | Opcode::LdindI
        | Opcode::LdindR4
        | Opcode::LdindR8
        | Opcode::LdindRef => match frame.pop()? {
            Value::ByRef(Location::Stack {
                frame: owner,
                buffer,
                offset,
            }) => {
                let width = indirect_width(opcode).ok_or(Trap::TypeMismatch(opcode))?;
                if owner != frame_index {
                    return Ok(Flow::LoadStack {
                        owner,
                        buffer,
                        offset,
                        opcode,
                    });
                }
                let raw = frame
                    .read_stack(buffer, offset, width)
                    .ok_or(Trap::NullReference)?;
                frame.stack.push(stack_loaded_value(opcode, raw)?);
            }
            Value::ByRef(location) => return Ok(Flow::LoadObj { location, opcode, token: Token(0) }),
            Value::Null => return Err(Trap::NullReference),
            _ => return Err(Trap::TypeMismatch(Opcode::LdindI4)),
        },
        // stind.i1 / stind.i2 store the value's low 8 / 16 bits, as a store into a 1- or 2-byte location
        // truncates to fit (ECMA-335 6th ed I.12.1.2 and III.3.62); raw `stackalloc` memory takes exactly
        // those bytes, and so does a local or an argument whose declared kind the loader recorded, into
        // its declared bytes ([`store_into_declared_slot`]), and an array whose elements are packed, into
        // its bytes at the address ([`store_into_packed_element`]). Any other value slot takes the value
        // as an `int32` in place of its whole value ([`narrow_stored_int`]). stind.r4 / stind.r8 store a float32 / float64, rounding a floating
        // value of the other width (ECMA-335 6th ed III.3.62) and refusing any other value
        // ([`stored_at_instruction_width`]); the others store the value as it is.
        Opcode::StindI1 | Opcode::StindI2 => {
            let value = frame.pop()?;
            match frame.pop()? {
                Value::ByRef(Location::Stack {
                    frame: owner,
                    buffer,
                    offset,
                }) => {
                    if owner != frame_index {
                        return Ok(Flow::StoreStack {
                            owner,
                            buffer,
                            offset,
                            opcode,
                            value,
                        });
                    }
                    let location = Location::Stack {
                        frame: owner,
                        buffer,
                        offset,
                    };
                    store_stack_indirect(frame, frame_index, opcode, location, value)?;
                }
                Value::ByRef(location) => {
                    let value = narrow_stored_int(value, opcode);
                    return Ok(Flow::StoreObj { location, value, opcode, token: Token(0) });
                }
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::StindI4)),
            }
        }
        Opcode::StindI4
        | Opcode::StindI8
        | Opcode::StindI
        | Opcode::StindR4
        | Opcode::StindR8
        | Opcode::StindRef => {
            let value = frame.pop()?;
            match frame.pop()? {
                Value::ByRef(Location::Stack {
                    frame: owner,
                    buffer,
                    offset,
                }) => {
                    if owner != frame_index {
                        return Ok(Flow::StoreStack {
                            owner,
                            buffer,
                            offset,
                            opcode,
                            value,
                        });
                    }
                    let location = Location::Stack {
                        frame: owner,
                        buffer,
                        offset,
                    };
                    store_stack_indirect(frame, frame_index, opcode, location, value)?;
                }
                Value::ByRef(location) => {
                    let value = stored_at_instruction_width(value, opcode)?;
                    return Ok(Flow::StoreObj { location, value, opcode, token: Token(0) });
                }
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::StindI4)),
            }
        }
        Opcode::Ldtoken => {
            let token = token_operand(instruction)?;
            frame
                .stack
                .push(Value::NativeInt(asm_key(asm, token.0) as i64));
        }

        Opcode::Ldsfld => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldsfld))?;
            let token = token_operand(instruction)?;
            let slot = module
                .static_field_slot(asm, token)
                .ok_or_else(|| unresolved_field(module, asm, token))?;
            if let Some(type_id) = module.type_of_static_slot(slot) {
                if let Some(cctor) = vm.take_pending_cctor(module, type_id)
                    .map_err(Trap::TypeInitializationFailed)? {
                    return Ok(Flow::EnsureCctor(cctor));
                }
            }
            if vm.statics_len() < module.static_field_count() {
                vm.init_statics(&module.decode_static_defaults());
            }
            let value = vm.static_field(slot).ok_or(Trap::FieldSlotMissing { field: token, slot: slot as u32 })?;
            frame.stack.push(value);
        }
        Opcode::Stsfld => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Stsfld))?;
            let token = token_operand(instruction)?;
            let slot = module
                .static_field_slot(asm, token)
                .ok_or_else(|| unresolved_field(module, asm, token))?;
            if let Some(type_id) = module.type_of_static_slot(slot) {
                if let Some(cctor) = vm.take_pending_cctor(module, type_id)
                    .map_err(Trap::TypeInitializationFailed)? {
                    return Ok(Flow::EnsureCctor(cctor));
                }
            }
            let value = frame.pop()?;
            let value = if depends_on_slot_kind(&value) { stored_at_storage(module.static_storage(slot), value) } else { value };
            if vm.statics_len() < module.static_field_count() {
                vm.init_statics(&module.decode_static_defaults());
            }
            vm.set_static_field(slot, value);
        }

        Opcode::Castclass => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Castclass))?;
            let token = token_operand(instruction)?;
            let value = frame.pop()?;
            if !cast_matches(module, asm, vm, &value, token) {
                return Err(Trap::InvalidCast);
            }
            frame.stack.push(value);
        }

        Opcode::Isinst => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Isinst))?;
            let token = token_operand(instruction)?;
            let value = frame.pop()?;
            let matched =
                !matches!(value, Value::Null) && cast_matches(module, asm, vm, &value, token);
            frame.stack.push(if matched { value } else { Value::Null });
        }

        Opcode::Box => {
            let token = token_operand(instruction)?;
            let value = frame.pop()?;
            if matches!(value, Value::Null | Value::Object(_)) {
                frame.stack.push(value);
                return Ok(Flow::Next);
            }
            if module.is_some_and(|module| {
                module
                    .type_id_of(asm, token)
                    .is_some_and(|type_id| !module.type_is_value_type(type_id))
            }) {
                frame.stack.push(value);
                return Ok(Flow::Next);
            }
            if let Some(underlying) =
                module.and_then(|module| nullable_underlying_of(module, asm, token))
            {
                let Value::Struct(fields) = &value else {
                    return Err(Trap::TypeMismatch(Opcode::Box));
                };
                let (Some(has_value), Some(inner)) = (fields.first(), fields.get(1)) else {
                    return Err(Trap::TypeMismatch(Opcode::Box));
                };
                if !has_value.is_truthy() {
                    frame.stack.push(Value::Null);
                    return Ok(Flow::Next);
                }
                let inner = inner.clone();
                check_alloc_budget(vm)?;
                let reference = vm.heap_mut().alloc_boxed(underlying, inner);
                frame.stack.push(Value::Object(reference));
                return Ok(Flow::Next);
            }
            check_alloc_budget(vm)?;
            let reference = vm.heap_mut().alloc_boxed(asm_key(asm, token.0), value);
            frame.stack.push(Value::Object(reference));
        }
        Opcode::Unbox => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Unbox))?;
            let token = token_operand(instruction)?;
            if let Some(underlying) = nullable_underlying_of(module, asm, token) {
                let source = frame.pop()?;
                let value = nullable_from_boxed(module, vm, asm, token, underlying, &source)
                    .ok_or(Trap::InvalidCast)?;
                check_alloc_budget(vm)?;
                let object = vm.heap_mut().alloc_boxed(asm_key(asm, token.0), value);
                frame.stack.push(Value::ByRef(Location::Boxed { object }));
                return Ok(Flow::Next);
            }
            let reference = object_ref(frame.pop()?, Opcode::Unbox)?;
            if !unbox_matches(module, asm, vm, reference, token) {
                return Err(Trap::InvalidCast);
            }
            frame
                .stack
                .push(Value::ByRef(Location::Boxed { object: reference }));
        }
        Opcode::UnboxAny => {
            let module = module.ok_or(Trap::Unsupported(Opcode::UnboxAny))?;
            let token = token_operand(instruction)?;
            if let Some(underlying) = nullable_underlying_of(module, asm, token) {
                let source = frame.pop()?;
                let value = nullable_from_boxed(module, vm, asm, token, underlying, &source)
                    .ok_or(Trap::InvalidCast)?;
                frame.stack.push(value);
                return Ok(Flow::Next);
            }
            let popped = frame.pop()?;
            if matches!(popped, Value::Null) && is_value_type_operand(module, asm, token) == Some(false) {
                frame.stack.push(Value::Null);
                return Ok(Flow::Next);
            }
            let reference = object_ref(popped, Opcode::UnboxAny)?;
            match vm.heap().boxed_value(reference) {
                Some(value) => {
                    if !unbox_matches(module, asm, vm, reference, token) {
                        return Err(Trap::InvalidCast);
                    }
                    frame.stack.push(value);
                }
                None => {
                    let value = Value::Object(reference);
                    if !cast_matches(module, asm, vm, &value, token) {
                        return Err(Trap::InvalidCast);
                    }
                    frame.stack.push(value);
                }
            }
        }

        Opcode::Newarr => {
            check_alloc_budget(vm)?;
            let module = module.ok_or(Trap::Unsupported(Opcode::Newarr))?;
            let token = token_operand(instruction)?;
            let length = array_length(frame.pop()?)?;
            let object = alloc_array_of_element(vm, module, asm, token, length);
            frame.stack.push(Value::Object(object));
        }

        Opcode::Ldlen => {
            let array = object_ref(frame.pop()?, Opcode::Ldlen)?;
            let length = vm.heap().array_len(array).ok_or(Trap::NullReference)?;
            frame.stack.push(Value::NativeInt(length as i64));
        }

        Opcode::LdelemI1
        | Opcode::LdelemU1
        | Opcode::LdelemI2
        | Opcode::LdelemU2
        | Opcode::LdelemI4
        | Opcode::LdelemU4
        | Opcode::LdelemI8
        | Opcode::LdelemI
        | Opcode::LdelemR4
        | Opcode::LdelemR8
        | Opcode::LdelemRef
        | Opcode::Ldelem => {
            let index = array_index(frame.pop()?, instruction.opcode)?;
            let array = object_ref(frame.pop()?, instruction.opcode)?;
            let len = vm.heap().array_len(array).ok_or(Trap::NullReference)?;
            let index = bounded_index(index, len)?;
            let value = vm
                .heap()
                .array_get(array, index)
                .ok_or(Trap::IndexOutOfRange(index as i32))?;
            if instruction.opcode == Opcode::Ldelem
                && matches!(value, Value::Object(_))
                && vm.heap().array_element_type(array) == Some(0)
                && module.is_some_and(|module| {
                    token_operand(instruction)
                        .is_ok_and(|token| is_value_type_operand(module, asm, token) == Some(true))
                })
            {
                return Err(Trap::TypeMismatch(Opcode::Ldelem));
            }
            let value = match (instruction.opcode, value) {
                (Opcode::LdelemU1, Value::Int32(raw)) => Value::Int32(raw & 0xFF),
                (Opcode::LdelemI1, Value::Int32(raw)) => Value::Int32(i32::from(raw as u8 as i8)),
                (Opcode::LdelemU2, Value::Int32(raw)) => Value::Int32(raw & 0xFFFF),
                (Opcode::LdelemI2, Value::Int32(raw)) => Value::Int32(i32::from(raw as u16 as i16)),
                (_, value) => value,
            };
            frame.stack.push(value);
        }

        Opcode::StelemI1
        | Opcode::StelemI2
        | Opcode::StelemI4
        | Opcode::StelemI8
        | Opcode::StelemI
        | Opcode::StelemR4
        | Opcode::StelemR8
        | Opcode::StelemRef
        | Opcode::Stelem => {
            let value = frame.pop()?;
            let index = array_index(frame.pop()?, instruction.opcode)?;
            let array = object_ref(frame.pop()?, instruction.opcode)?;
            let len = vm.heap().array_len(array).ok_or(Trap::NullReference)?;
            let index = bounded_index(index, len)?;
            if matches!(instruction.opcode, Opcode::StelemRef | Opcode::Stelem) {
                if let (Value::Object(_), Some(element_type)) =
                    (&value, vm.heap().array_element_type(array))
                {
                    let exact_element = instruction.opcode == Opcode::Stelem
                        && module.is_some_and(|module| {
                            token_operand(instruction)
                                .is_ok_and(|token| array_element_is(module, element_type, asm, token))
                        });
                    if element_type != 0 && !exact_element {
                        let element_asm = (element_type >> 32) as u8;
                        let element_token = Token((element_type & 0xFFFF_FFFF) as u32);
                        let matches = module.is_some_and(|module| {
                            module.is_object_type_token(element_asm, element_token)
                                || matches!(module.cast_elem(element_type), Some(CastElem::Object))
                                || cast_matches(module, element_asm, vm, &value, element_token)
                        });
                        if !matches {
                            return Err(Trap::ArrayTypeMismatch);
                        }
                    }
                }
            }
            if !vm.heap_mut().array_set(array, index, value) {
                return Err(Trap::IndexOutOfRange(index as i32));
            }
        }

        #[cfg(feature = "exceptions")]
        Opcode::Throw => {
            let exception = object_ref(frame.pop()?, Opcode::Throw)?;
            return Ok(Flow::Throw(exception));
        }
        #[cfg(feature = "exceptions")]
        Opcode::Rethrow => {
            let here = frame.ip.saturating_sub(1);
            let owner = match module {
                Some(module) => method_handlers(module, frame.method)?
                    .iter()
                    .filter(|clause| {
                        matches!(clause.kind, EhKind::Catch(_) | EhKind::Filter { .. })
                            && covers(clause.handler_range, here)
                    })
                    .min_by_key(|clause| clause.handler_range.end - clause.handler_range.start)
                    .map(|clause| clause.handler_range.start as usize),
                None => None,
            };
            let exception = match owner {
                Some(start) => frame
                    .caught_exceptions
                    .iter()
                    .find(|&&(handler, _)| handler == start),
                None => frame.caught_exceptions.last(),
            }
            .map(|&(_, exception)| exception)
            .ok_or(Trap::Unsupported(Opcode::Rethrow))?;
            return Ok(Flow::Throw(exception));
        }
        #[cfg(feature = "exceptions")]
        Opcode::Leave | Opcode::LeaveS => {
            return Ok(Flow::Leave(branch_target(instruction, code_len)?));
        }
        #[cfg(feature = "exceptions")]
        Opcode::Endfinally => return Ok(Flow::EndFinally),
        #[cfg(feature = "exceptions")]
        Opcode::Endfilter => {
            let accept = matches!(frame.pop()?, Value::Int32(n) if n != 0);
            return Ok(Flow::EndFilter(accept));
        }

        Opcode::Sizeof => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Sizeof))?;
            let token = token_operand(instruction)?;
            let size = module
                .type_size(asm, token)
                .ok_or(Trap::Unsupported(Opcode::Sizeof))?;
            frame.stack.push(Value::Int32(size as i32));
        }

        Opcode::Localloc => {
            let size = match frame.pop()? {
                Value::Int32(n) => n as u32 as usize,
                Value::Int64(n) | Value::NativeInt(n) => n as usize,
                _ => return Err(Trap::TypeMismatch(Opcode::Localloc)),
            };
            let pointer = frame.localloc(frame_index, size);
            frame.stack.push(pointer);
        }

        Opcode::Ret => return Ok(Flow::Return(frame.stack.pop())),

        #[cfg(feature = "typed-references")]
        Opcode::Mkrefany => {
            let token = token_operand(instruction)?;
            match frame.pop()? {
                Value::ByRef(location) => frame.stack.push(Value::TypedRef {
                    location,
                    type_token: asm_key(asm, token.0),
                }),
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Mkrefany)),
            }
        }
        #[cfg(feature = "typed-references")]
        Opcode::Refanyval => {
            let token = token_operand(instruction)?;
            match frame.pop()? {
                Value::TypedRef {
                    location,
                    type_token,
                } => {
                    let expected = asm_key(asm, token.0);
                    let matches = type_token == expected
                        || matches!(
                            module.and_then(|m| m
                                .type_name_by_handle(type_token)
                                .zip(m.type_name_by_handle(expected))),
                            Some((actual, wanted)) if actual == wanted
                        );
                    if !matches {
                        return Err(Trap::InvalidCast);
                    }
                    frame.stack.push(Value::ByRef(location));
                }
                _ => return Err(Trap::TypeMismatch(Opcode::Refanyval)),
            }
        }
        #[cfg(feature = "typed-references")]
        Opcode::Refanytype => match frame.pop()? {
            Value::TypedRef { type_token, .. } => {
                frame.stack.push(Value::NativeInt(type_token as i64));
            }
            _ => return Err(Trap::TypeMismatch(Opcode::Refanytype)),
        },
        #[cfg(feature = "varargs")]
        Opcode::Arglist => {
            let typedrefs: alloc::vec::Vec<Value> = match &frame.varargs {
                Some(vararg) => (0..vararg.types.len())
                    .map(|i| Value::TypedRef {
                        location: Location::Arg {
                            frame: frame_index,
                            slot: vararg.start as usize + i,
                        },
                        type_token: vararg.types[i],
                    })
                    .collect(),
                None => alloc::vec::Vec::new(),
            };
            let array = vm.heap_mut().alloc_array(typedrefs);
            frame.stack.push(Value::Object(array));
        }

        #[cfg(not(feature = "typed-references"))]
        op @ (Opcode::Mkrefany | Opcode::Refanyval | Opcode::Refanytype) => {
            return Err(Trap::Unsupported(op));
        }
        #[cfg(not(feature = "varargs"))]
        Opcode::Arglist => return Err(Trap::Unsupported(Opcode::Arglist)),

        Opcode::Initblk => {
            let size = block_size(frame.pop()?)?;
            let value = block_byte(frame.pop()?);
            match frame.pop()? {
                Value::ByRef(Location::Stack {
                    frame: owner,
                    buffer,
                    offset,
                }) if owner == frame_index => {
                    frame.fill_stack(buffer, offset, value, size)?;
                }
                Value::ByRef(Location::Element {
                    array,
                    index,
                    byte_offset,
                }) => {
                    let start = index + byte_offset as usize;
                    let slice = vm
                        .heap_mut()
                        .array_u8_slice_mut(array, start, size)
                        .ok_or(Trap::Unsupported(Opcode::Initblk))?;
                    slice.iter_mut().for_each(|byte| *byte = value);
                }
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::Unsupported(Opcode::Initblk)),
            }
        }
        Opcode::Cpblk => {
            let size = block_size(frame.pop()?)?;
            let source = frame.pop()?;
            match (frame.pop()?, source) {
                (
                    Value::ByRef(Location::Stack {
                        frame: dest_owner,
                        buffer: dest_buffer,
                        offset: dest_offset,
                    }),
                    Value::ByRef(Location::Stack {
                        frame: src_owner,
                        buffer: src_buffer,
                        offset: src_offset,
                    }),
                ) if dest_owner == frame_index && src_owner == frame_index => {
                    frame.copy_stack(dest_buffer, dest_offset, src_buffer, src_offset, size)?;
                }
                (
                    Value::ByRef(Location::Element {
                        array: dest_array,
                        index: dest_index,
                        byte_offset: dest_bo,
                    }),
                    Value::ByRef(Location::Element {
                        array: src_array,
                        index: src_index,
                        byte_offset: src_bo,
                    }),
                ) => {
                    let temp = vm
                        .heap()
                        .array_u8_slice(src_array, src_index + src_bo as usize, size)
                        .ok_or(Trap::Unsupported(Opcode::Cpblk))?
                        .to_vec();
                    let dest = vm
                        .heap_mut()
                        .array_u8_slice_mut(dest_array, dest_index + dest_bo as usize, size)
                        .ok_or(Trap::Unsupported(Opcode::Cpblk))?;
                    dest.copy_from_slice(&temp);
                }
                (Value::Null, _) | (_, Value::Null) => return Err(Trap::NullReference),
                _ => return Err(Trap::Unsupported(Opcode::Cpblk)),
            }
        }

        #[allow(unreachable_patterns)]
        other => return Err(Trap::Unsupported(other)),
    }
    Ok(Flow::Next)
}

impl Frame {
    /// Sizes the frame for its method's `clauses` exception clauses when it is set up, so that a
    /// CATCH only writes: an entry per handler in [`Frame::caught_exceptions`], and room on the
    /// evaluation stack for the exception the handler starts with. A catch is where a program
    /// meets an `OutOfMemoryException`, raised at the object budget with only the headroom below
    /// capacity left, so it must need no memory of its own. A method with no clauses is untouched,
    /// and a pooled shell that already has the room is not grown.
    fn prepare_for_handlers(&mut self, clauses: usize) {
        if clauses > 0 {
            self.caught_exceptions.reserve(clauses);
            self.stack.reserve(1);
        }
    }

    fn new(method: MethodId, context: MethodContext, args: Vec<Value>) -> Frame {
        Frame {
            method,
            context,
            ip: 0,
            fault_ip: 0,
            stack: Vec::new(),
            locals: Vec::new(),
            args,
            new_object: None,
            new_value: None,
            caught_exceptions: Vec::new(),
            pending: Vec::new(),
            pending_filter: None,
            multicast: None,
            pending_constraint: None,
            pending_readonly: false,
            buffers: Vec::new(),
            varargs: None,
        }
    }

    fn pop(&mut self) -> Result<Value, Trap> {
        self.stack.pop().ok_or(Trap::StackUnderflow)
    }

    /// Pops the two operands of a binary instruction, returning them in source
    /// order `(deeper, shallower)` -- the CLI's `value1, value2`.
    fn pop2(&mut self) -> Result<(Value, Value), Trap> {
        let second = self.pop()?;
        let first = self.pop()?;
        Ok((first, second))
    }

    /// Takes the top `count` values as call arguments, in declaration order:
    /// the deepest of the popped values is argument 0 (it was pushed first).
    fn take_args(&mut self, count: u16) -> Result<Vec<Value>, Trap> {
        let count = count as usize;
        if self.stack.len() < count {
            return Err(Trap::StackUnderflow);
        }
        Ok(self.stack.split_off(self.stack.len() - count))
    }

    fn load_arg(&mut self, slot: u16) -> Result<(), Trap> {
        let value = self
            .args
            .get(slot as usize)
            .ok_or(Trap::ArgumentOutOfRange(slot))?
            .clone();
        self.stack.push(value);
        Ok(())
    }

    fn load_local(&mut self, slot: u16) {
        let value = self
            .locals
            .get(slot as usize)
            .cloned()
            .unwrap_or(Value::Int32(0));
        self.stack.push(value);
    }

    fn store_local(&mut self, slot: u16) -> Result<(), Trap> {
        let value = self.pop()?;
        if self.locals.len() <= slot as usize {
            self.locals.resize(slot as usize + 1, Value::Int32(0));
        }
        self.locals[slot as usize] = value;
        Ok(())
    }

    fn store_arg(&mut self, slot: u16) -> Result<(), Trap> {
        let value = self.pop()?;
        let target = self
            .args
            .get_mut(slot as usize)
            .ok_or(Trap::ArgumentOutOfRange(slot))?;
        *target = value;
        Ok(())
    }

    /// `localloc` (III.3.47): allocate a zeroed buffer of `size` bytes in this frame and
    /// return a managed pointer to its start. C# zero-initializes `stackalloc`, so the
    /// block is zeroed; it is freed when the frame returns.
    fn localloc(&mut self, frame_index: usize, size: usize) -> Value {
        let buffer = self.buffers.len();
        self.buffers.push(alloc::vec![0u8; size]);
        Value::ByRef(Location::Stack {
            frame: frame_index,
            buffer,
            offset: 0,
        })
    }

    /// Reads `width` bytes (little-endian) at `offset` of localloc buffer `buffer`, or
    /// `None` if the access is out of range or the buffer does not exist. Reads beyond the
    /// last written byte see the zero-initialized fill.
    fn read_stack(&self, buffer: usize, offset: u32, width: usize) -> Option<[u8; 8]> {
        let bytes = self.buffers.get(buffer)?;
        let start = offset as usize;
        let end = start.checked_add(width)?;
        let slice = bytes.get(start..end)?;
        let mut out = [0u8; 8];
        out[..width].copy_from_slice(slice);
        Some(out)
    }

    /// Writes `width` little-endian bytes of `value` at `offset` of localloc buffer
    /// `buffer`. Errors (a wild-pointer trap) if the access is out of range or the buffer
    /// does not exist.
    fn write_stack(
        &mut self,
        buffer: usize,
        offset: u32,
        value: [u8; 8],
        width: usize,
    ) -> Result<(), Trap> {
        let bytes = self.buffers.get_mut(buffer).ok_or(Trap::NullReference)?;
        let start = offset as usize;
        let end = start.checked_add(width).ok_or(Trap::NullReference)?;
        let slice = bytes.get_mut(start..end).ok_or(Trap::NullReference)?;
        slice.copy_from_slice(&value[..width]);
        Ok(())
    }

    /// `initblk`: sets `size` bytes of localloc buffer `buffer` from `offset` to `value`. A
    /// wild-pointer trap if the range is out of bounds or the buffer does not exist.
    fn fill_stack(&mut self, buffer: usize, offset: u32, value: u8, size: usize) -> Result<(), Trap> {
        let bytes = self.buffers.get_mut(buffer).ok_or(Trap::NullReference)?;
        let start = offset as usize;
        let end = start.checked_add(size).ok_or(Trap::NullReference)?;
        let slice = bytes.get_mut(start..end).ok_or(Trap::NullReference)?;
        slice.iter_mut().for_each(|byte| *byte = value);
        Ok(())
    }

    /// `cpblk`: copies `size` bytes between localloc buffers of this frame (the source read into a
    /// temporary first, so an overlapping same-buffer copy is well-defined). A wild-pointer trap if
    /// either range is out of bounds or a buffer does not exist.
    fn copy_stack(
        &mut self,
        dest_buffer: usize,
        dest_offset: u32,
        src_buffer: usize,
        src_offset: u32,
        size: usize,
    ) -> Result<(), Trap> {
        let source = self.buffers.get(src_buffer).ok_or(Trap::NullReference)?;
        let src_start = src_offset as usize;
        let src_end = src_start.checked_add(size).ok_or(Trap::NullReference)?;
        let temp = source
            .get(src_start..src_end)
            .ok_or(Trap::NullReference)?
            .to_vec();
        let dest = self.buffers.get_mut(dest_buffer).ok_or(Trap::NullReference)?;
        let dest_start = dest_offset as usize;
        let dest_end = dest_start.checked_add(size).ok_or(Trap::NullReference)?;
        let slice = dest.get_mut(dest_start..dest_end).ok_or(Trap::NullReference)?;
        slice.copy_from_slice(&temp);
        Ok(())
    }
}

/// The byte count of a `cpblk` / `initblk` (the `size` operand: a native int or int32, taken
/// non-negative). An out-of-range or non-integer size is a type mismatch.
fn block_size(value: Value) -> Result<usize, Trap> {
    let count = match value {
        Value::Int32(n) => i64::from(n),
        Value::NativeInt(n) => n,
        _ => return Err(Trap::TypeMismatch(Opcode::Cpblk)),
    };
    usize::try_from(count).map_err(|_| Trap::TypeMismatch(Opcode::Cpblk))
}

/// The fill byte of an `initblk` (the `value` operand: the low 8 bits of an int32 / native int).
fn block_byte(value: Value) -> u8 {
    match value {
        Value::Int32(n) => n as u8,
        Value::NativeInt(n) => n as u8,
        _ => 0,
    }
}

/// The byte width an `ldind.*` / `stind.*` opcode transfers (III.3.42-43), or `None`
/// for `ldind.ref` / `stind.ref`, which move an object reference rather than raw bytes
/// (a `Location::Stack` pointer carries no object reference, so those do not apply to it).
fn indirect_width(opcode: Opcode) -> Option<usize> {
    indirect_width_at(opcode, crate::native_pointer_size() as usize)
}

/// [`indirect_width`] for a target whose native int is `native` bytes wide: `ldind.i` and
/// `stind.i` move a native int, as wide as the target maps it (ECMA-335 6th ed I.12.1.1).
fn indirect_width_at(opcode: Opcode, native: usize) -> Option<usize> {
    match opcode {
        Opcode::LdindI1 | Opcode::LdindU1 | Opcode::StindI1 => Some(1),
        Opcode::LdindI2 | Opcode::LdindU2 | Opcode::StindI2 => Some(2),
        Opcode::LdindI4 | Opcode::LdindU4 | Opcode::StindI4 => Some(4),
        Opcode::LdindI8 | Opcode::StindI8 => Some(8),
        Opcode::LdindI | Opcode::StindI => Some(native),
        #[cfg(feature = "float")]
        Opcode::LdindR4 | Opcode::StindR4 => Some(4),
        #[cfg(feature = "float")]
        Opcode::LdindR8 | Opcode::StindR8 => Some(8),
        _ => None,
    }
}

/// Decodes the little-endian bytes an `ldind.*` read from a `Location::Stack` into the
/// stack value the typed opcode yields -- sign- or zero-extending a sub-`int32` integer
/// to `int32` (III.1.6), and producing the wider integer / native / float types directly.
fn stack_loaded_value(opcode: Opcode, raw: [u8; 8]) -> Result<Value, Trap> {
    stack_loaded_value_at(opcode, raw, crate::native_pointer_size() as usize)
}

/// [`stack_loaded_value`] for a target whose native int is `native` bytes wide (ECMA-335 6th ed
/// I.12.1.1). The interpreter keeps a native int in 64 bits, and on a 4-byte target the same 32-bit
/// value has two forms there: `conv.u` zero-extends it and `conv.i` sign-extends it (ECMA-335 6th ed
/// III.3.27). Four bytes below 0x80000000 are the same value in both forms; four bytes with the top
/// bit set cannot be given back in the form they were stored in, so their `ldind.i` is refused.
fn stack_loaded_value_at(opcode: Opcode, raw: [u8; 8], native: usize) -> Result<Value, Trap> {
    let low8 = u64::from_le_bytes(raw);
    if opcode == Opcode::LdindI && native == 4 {
        let word = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        if word & 0x8000_0000 != 0 {
            return Err(Trap::Unsupported(opcode));
        }
        return Ok(Value::NativeInt(i64::from(word)));
    }
    Ok(match opcode {
        Opcode::LdindI1 => Value::Int32(i32::from(raw[0] as i8)),
        Opcode::LdindU1 => Value::Int32(i32::from(raw[0])),
        Opcode::LdindI2 => Value::Int32(i32::from(i16::from_le_bytes([raw[0], raw[1]]))),
        Opcode::LdindU2 => Value::Int32(i32::from(u16::from_le_bytes([raw[0], raw[1]]))),
        Opcode::LdindI4 => Value::Int32(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])),
        Opcode::LdindU4 => Value::Int32(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as i32),
        Opcode::LdindI8 => Value::Int64(low8 as i64),
        Opcode::LdindI => Value::NativeInt(low8 as i64),
        #[cfg(feature = "float")]
        Opcode::LdindR4 => Value::Single(f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])),
        #[cfg(feature = "float")]
        Opcode::LdindR8 => Value::Float(f64::from_le_bytes(raw)),
        _ => return Err(Trap::TypeMismatch(opcode)),
    })
}

/// Encodes the stack value a `stind.*` stores into its low `width` little-endian bytes
/// (III.3.62) for a write into a `Location::Stack` buffer. The store opcode fixes the
/// width; the value is taken at that width (a wider integer is truncated, matching how
/// `stind.i1`/`i2` narrow).
fn stack_stored_bytes(opcode: Opcode, value: Value) -> Result<[u8; 8], Trap> {
    let raw = match opcode {
        #[cfg(feature = "float")]
        Opcode::StindR4 => match value {
            Value::Single(f) => u64::from(f.to_bits()),
            Value::Float(f) => u64::from(f32::to_bits(f as f32)),
            _ => return Err(Trap::TypeMismatch(opcode)),
        },
        #[cfg(feature = "float")]
        Opcode::StindR8 => match value {
            Value::Single(f) => f64::from(f).to_bits(),
            Value::Float(f) => f.to_bits(),
            _ => return Err(Trap::TypeMismatch(opcode)),
        },
        _ => match value {
            Value::Int32(n) => n as u32 as u64,
            Value::Int64(n) | Value::NativeInt(n) => n as u64,
            _ => return Err(Trap::TypeMismatch(opcode)),
        },
    };
    Ok(raw.to_le_bytes())
}

/// Performs a `stind.*` through a `localloc` pointer: writes the value's low bytes into
/// the owning frame's buffer at the opcode's width (little-endian). The pointer must name
/// this frame's buffer (the common same-frame `stackalloc` case); a pointer that escaped
/// to another frame is unsupported (it cannot reach that frame's storage from here).
fn store_stack_indirect(
    frame: &mut Frame,
    frame_index: usize,
    opcode: Opcode,
    location: Location,
    value: Value,
) -> Result<(), Trap> {
    let Location::Stack {
        frame: owner,
        buffer,
        offset,
    } = location
    else {
        return Err(Trap::TypeMismatch(opcode));
    };
    let width = indirect_width(opcode).ok_or(Trap::TypeMismatch(opcode))?;
    if owner != frame_index {
        return Err(Trap::Unsupported(opcode));
    }
    let bytes = stack_stored_bytes(opcode, value)?;
    frame.write_stack(buffer, offset, bytes, width)
}

/// The unsigned index a `switch` pops, or `None` if the value is not an integer
/// (III.3.66 compares the value as an unsigned integer).
fn unsigned_index(value: Value) -> Option<usize> {
    match value {
        Value::Int32(x) => Some(x as u32 as usize),
        Value::Int64(x) | Value::NativeInt(x) => Some(x as usize),
        _ => None,
    }
}

/// The integer category of a stack value, for the operand-type rules of III.1.5.
#[derive(Clone, Copy, PartialEq)]
enum IntKind {
    I32,
    I64,
    Native,
}

fn int_parts(value: Value) -> Option<(i64, IntKind)> {
    match value {
        Value::Int32(value) => Some((i64::from(value), IntKind::I32)),
        Value::Int64(value) => Some((value, IntKind::I64)),
        Value::NativeInt(value) => Some((value, IntKind::Native)),
        _ => None,
    }
}

/// Combines the operand categories of a binary integer op into the result
/// category, or `None` if the pairing is invalid (III.1.5, Tables 11 and 14):
/// `int32` and `int64` may not mix, but either may combine with native `int`.
fn combine(a: IntKind, b: IntKind) -> Option<IntKind> {
    match (a, b) {
        (IntKind::I32, IntKind::I32) => Some(IntKind::I32),
        (IntKind::I64, IntKind::I64) => Some(IntKind::I64),
        (IntKind::Native, IntKind::Native) => Some(IntKind::Native),
        (IntKind::I32, IntKind::Native) | (IntKind::Native, IntKind::I32) => Some(IntKind::Native),
        _ => None,
    }
}

fn wrap_int(kind: IntKind, value: i64) -> Value {
    match kind {
        IntKind::I32 => Value::Int32(value as i32),
        IntKind::I64 => Value::Int64(value),
        IntKind::Native => Value::NativeInt(value),
    }
}

/// `add`, `sub`, `mul`, `div`, `rem`: floating point when both operands are
/// floats, otherwise integer.
///
/// `float32 op float32` computes at `f32` precision (so the rounded single-precision result
/// matches .NET); any operand of double precision lifts the other to `f64` and computes there
/// (ECMA-335 III.1.5: the F type carries implementation-defined precision, and C# inserts the
/// `conv.r8` for a mixed `float`/`double` expression, so widening here yields the same value).
fn binary_numeric(opcode: Opcode, a: Value, b: Value) -> Result<Value, Trap> {
    #[cfg(feature = "float")]
    if let (Value::Single(x), Value::Single(y)) = (&a, &b) {
        let (x, y) = (*x, *y);
        let result = match opcode {
            Opcode::Add => x + y,
            Opcode::Sub => x - y,
            Opcode::Mul => x * y,
            Opcode::Div => x / y,
            Opcode::Rem => x % y,
            _ => return Err(Trap::Unsupported(opcode)),
        };
        return Ok(Value::Single(result));
    }
    #[cfg(feature = "float")]
    if let (Some(x), Some(y)) = (as_double(&a), as_double(&b)) {
        let result = match opcode {
            Opcode::Add => x + y,
            Opcode::Sub => x - y,
            Opcode::Mul => x * y,
            Opcode::Div => x / y,
            Opcode::Rem => x % y,
            _ => return Err(Trap::Unsupported(opcode)),
        };
        return Ok(Value::Float(result));
    }
    binary_integer(opcode, a, b)
}

/// The `f64` value of a floating-point stack value (a `Single` widened, a `Float` directly), or
/// `None` for a non-float -- the seam mixed-precision float arithmetic and comparison share.
#[cfg(feature = "float")]
fn as_double(value: &Value) -> Option<f64> {
    match value {
        Value::Float(x) => Some(*x),
        Value::Single(x) => Some(f64::from(*x)),
        _ => None,
    }
}

/// The integer offset of an `add`/`sub` operand, if it is one (for stepping a localloc
/// pointer). A managed pointer is not an offset.
fn pointer_offset(value: &Value) -> Option<i64> {
    match value {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) | Value::NativeInt(n) => Some(*n),
        _ => None,
    }
}

/// `add`/`sub` of a raw byte-addressed pointer and an integer byte offset, yielding the
/// adjusted pointer. This is the pointer walk behind C# `stackalloc` indexing (a
/// `Location::Stack` localloc pointer) and behind `fixed`-array indexing (a `Location::Element`
/// pointer into a pinned array, whose byte offset an eventual `ldind`/`stind` divides by the
/// element width). Returns `Ok(None)` when neither operand is such a pointer, so the caller
/// falls back to ordinary numeric arithmetic; `add` accepts the offset on either side (it is
/// commutative), `sub` as `pointer - offset` and as `pointer - pointer` (III.1.5). The latter
/// yields the signed BYTE difference as a native int (C# then divides by `sizeof(T)` for the
/// element count), defined only for two pointers into the same buffer/array.
///
/// Only the UNSIGNED overflow-checked forms take a pointer operand: Table III.7 gives `&` a
/// result only under `add.ovf.un` and `sub.ovf.un`, so a pointer under the signed `add.ovf`/
/// `sub.ovf` is invalid CIL and falls through here to the integer path, which rejects it.
///
/// # The overflow model for the checked forms
///
/// `add.ovf.un`/`sub.ovf.un` read both operands as unsigned native ints and throw when the
/// result "cannot be represented in the result type" (III.3.2, III.3.65). On a flat address
/// space that means the ADDRESS carried out of the pointer width. A pointer here has no address:
/// it is a `Location` plus a byte offset within ONE allocation, which is the model III.1.5
/// prescribes -- it defines pointer arithmetic only within a single array and leaves every other
/// use unspecified, precisely because a moving memory manager makes absolute addresses and
/// inter-object distances unstable. This collector is moving-capable, so an address would be a
/// fiction.
///
/// So a pointer's unsigned native value here IS its byte offset, and its representable range is
/// its allocation's offset space -- `[0, 2^32)`, the target's pointer width (`TargetLayout::ilp32`,
/// what `sizeof(IntPtr)` reports and what the `u32` offsets already encode). The check is that
/// offset arithmetic carrying or borrowing out of the range. Equivalently: the allocation is
/// modelled as based at 0. That is the only choice that is deterministic and independent of
/// allocation order -- a synthetic base address would make a program's behaviour depend on heap
/// layout.
///
/// Against a real flat-address device the model is exact for `pointer - pointer` (the base
/// cancels), LOOSER by the base for `pointer + offset` (a device based at B carries where we do
/// not), and STRICTER by the base for `pointer - offset` (we throw walking below the allocation
/// base, where a device would not). All three residues lie outside a single allocation, which is
/// exactly the region III.1.5 leaves unspecified; in-bounds arithmetic agrees exactly.
fn stack_pointer_arithmetic(
    heap: &Heap,
    opcode: Opcode,
    a: &Value,
    b: &Value,
) -> Result<Option<Value>, Trap> {
    let add = matches!(opcode, Opcode::Add | Opcode::AddOvfUn);
    let sub = matches!(opcode, Opcode::Sub | Opcode::SubOvfUn);
    let checked = matches!(opcode, Opcode::AddOvfUn | Opcode::SubOvfUn);
    if !add && !sub {
        return Ok(None);
    }
    if sub {
        if let (Value::ByRef(la), Value::ByRef(lb)) = (a, b) {
            if is_raw_pointer(la) && is_raw_pointer(lb) {
                let difference = raw_pointer_byte_difference(heap, la, lb);
                if checked && difference.is_some_and(i64::is_negative) {
                    return Err(Trap::Overflow);
                }
                return Ok(difference.map(Value::NativeInt));
            }
        }
    }
    let (location, offset) = match (a, b) {
        (Value::ByRef(location), other) if is_raw_pointer(location) => {
            let Some(delta) = pointer_offset(other) else {
                return Ok(None);
            };
            (location, delta)
        }
        (other, Value::ByRef(location)) if add && is_raw_pointer(location) => {
            let Some(delta) = pointer_offset(other) else {
                return Ok(None);
            };
            (location, delta)
        }
        _ => return Ok(None),
    };
    let signed = if sub { offset.wrapping_neg() } else { offset };
    let walked = |base: u32| -> Result<u32, Trap> {
        if !checked {
            return Ok(i64::from(base).wrapping_add(signed) as u32);
        }
        let delta = offset as u32;
        let result = if sub {
            base.checked_sub(delta)
        } else {
            base.checked_add(delta)
        };
        result.ok_or(Trap::Overflow)
    };
    let stepped = match location {
        Location::Stack {
            frame,
            buffer,
            offset: base,
        } => Location::Stack {
            frame: *frame,
            buffer: *buffer,
            offset: walked(*base)?,
        },
        Location::Element {
            array,
            index,
            byte_offset,
        } => Location::Element {
            array: *array,
            index: *index,
            byte_offset: walked(*byte_offset)?,
        },
        Location::StringChar {
            string,
            byte_offset,
        } => Location::StringChar {
            string: *string,
            byte_offset: walked(*byte_offset)?,
        },
        Location::Local { frame, slot } => Location::LocalBytes {
            frame: *frame,
            slot: *slot,
            byte_offset: walked(0)?,
        },
        Location::Arg { frame, slot } => Location::ArgBytes {
            frame: *frame,
            slot: *slot,
            byte_offset: walked(0)?,
        },
        Location::LocalBytes {
            frame,
            slot,
            byte_offset,
        } => Location::LocalBytes {
            frame: *frame,
            slot: *slot,
            byte_offset: walked(*byte_offset)?,
        },
        Location::ArgBytes {
            frame,
            slot,
            byte_offset,
        } => Location::ArgBytes {
            frame: *frame,
            slot: *slot,
            byte_offset: walked(*byte_offset)?,
        },
        Location::Static { slot } => Location::StaticBytes {
            slot: *slot,
            byte_offset: walked(0)?,
        },
        Location::StaticBytes { slot, byte_offset } => Location::StaticBytes {
            slot: *slot,
            byte_offset: walked(*byte_offset)?,
        },
        _ => return Ok(None),
    };
    Ok(Some(Value::ByRef(stepped)))
}

/// `add` / `sub` on the address of an object's field, which unsafe C# walks to reach the fields
/// after it: `p[1]` after `fixed (int* p = &o.A)` is the field that starts four bytes past `A`.
///
/// An object's fields are typed slots here, not bytes, so the walk is resolved through the byte
/// layout the loader recorded for the object's type ([`Module::type_field_offsets`]). The result
/// is the address of the field that starts at the byte the arithmetic reaches, or the end of the
/// fields, which C# may form without dereferencing (a loop's end pointer). `pointer - pointer`
/// between two field addresses of one object is their distance in bytes. Any other byte -- inside
/// a field, on a struct-typed field, outside the object -- is refused by name rather than
/// addressed whole. The fields walked are a class instance's and a boxed struct's.
fn field_pointer_arithmetic(
    module: Option<&Module>,
    vm: &Vm,
    opcode: Opcode,
    a: &Value,
    b: &Value,
) -> Result<Option<Value>, Trap> {
    let add = matches!(opcode, Opcode::Add | Opcode::AddOvfUn);
    let sub = matches!(opcode, Opcode::Sub | Opcode::SubOvfUn);
    if !add && !sub {
        return Ok(None);
    }
    if sub {
        if let (Value::ByRef(la), Value::ByRef(lb)) = (a, b) {
            if is_field_address(la) && is_field_address(lb) {
                let module = module.ok_or(Trap::TypeMismatch(opcode))?;
                let (container_a, at_a) = field_walk_start(module, vm, la)?;
                let (container_b, at_b) = field_walk_start(module, vm, lb)?;
                if container_a.object != container_b.object {
                    return Err(Trap::TypeMismatch(opcode));
                }
                return Ok(Some(Value::NativeInt(i64::from(at_a) - i64::from(at_b))));
            }
        }
    }
    let (location, offset) = match (a, b) {
        (Value::ByRef(location), other) if is_field_address(location) => (location, other),
        (other, Value::ByRef(location)) if add && is_field_address(location) => (location, other),
        _ => return Ok(None),
    };
    let Some(delta) = pointer_offset(offset) else {
        return Ok(None);
    };
    let module = module.ok_or(Trap::TypeMismatch(opcode))?;
    let (container, start) = field_walk_start(module, vm, location)?;
    let signed = if sub { delta.wrapping_neg() } else { delta };
    let name = || module.type_full_name(container.owner).unwrap_or("<unknown>");
    let target = i64::from(start) + signed;
    let layout = container.layout;
    let slot = u32::try_from(target)
        .ok()
        .and_then(|byte| layout.slot_at(byte))
        .ok_or_else(|| {
            Trap::FieldPointerWalk(Cow::Owned(alloc::format!(
                "pointer arithmetic moved the address of a field of `{}` to byte {target}, where none \
                 of its fields starts (they span bytes 0 to {}); the interpreter addresses a field \
                 only whole",
                name(),
                layout.end()
            )))
        })?;
    if matches!(container.fields.get(slot), Some(Value::Struct(_))) {
        return Err(Trap::FieldPointerWalk(Cow::Owned(alloc::format!(
            "pointer arithmetic moved the address of a field of `{}` to the struct-typed field at \
             byte {target}; the interpreter cannot reach the fields inside it through that pointer",
            name()
        ))));
    }
    let slot = slot as u32;
    Ok(Some(Value::ByRef(match location {
        Location::Field { object, .. } => Location::Field {
            object: *object,
            slot,
        },
        Location::Nested { base, .. } => Location::Nested {
            base: base.clone(),
            slot,
        },
        _ => return Ok(None),
    })))
}

/// Whether `location` is the address of a field, the kind [`field_pointer_arithmetic`] walks.
fn is_field_address(location: &Location) -> bool {
    matches!(location, Location::Field { .. } | Location::Nested { .. })
}

/// The storage a field address points into, for [`field_pointer_arithmetic`].
struct FieldContainer<'v, 'm> {
    /// The object holding the fields: the instance, or the box.
    object: ObjectRef,
    /// The type whose layout numbers them.
    owner: TypeId,
    /// That layout.
    layout: FieldOffsets<'m>,
    /// The field slots themselves.
    fields: &'v [Value],
}

/// The fields a field address points into and the byte its field starts at, or the refusal that
/// says why that address cannot be walked.
fn field_walk_start<'v, 'm>(
    module: &'m Module,
    vm: &'v Vm,
    location: &Location,
) -> Result<(FieldContainer<'v, 'm>, u32), Trap> {
    let (object, slot) = match location {
        Location::Field { object, slot } => (*object, *slot),
        Location::Nested { base, slot } => match base.as_ref() {
            Location::Boxed { object } => (*object, *slot),
            _ => {
                return Err(Trap::FieldPointerWalk(Cow::Borrowed(
                    "pointer arithmetic on the address of a field of a struct held in a local, an \
                     argument, a static, an array element or another object's field: the \
                     interpreter walks the fields of a class instance or a boxed struct only",
                )));
            }
        },
        _ => return Err(Trap::TypeMismatch(Opcode::Add)),
    };
    let (owner, fields) = match vm.heap().get(object) {
        Some(crate::object::Object::Instance { type_id, fields }) => (*type_id, &fields[..]),
        Some(crate::object::Object::Boxed {
            type_token,
            value: Value::Struct(fields),
        }) => (
            module
                .type_id_by_handle(*type_token)
                .ok_or(Trap::TypeMismatch(Opcode::Add))?,
            &fields[..],
        ),
        _ => return Err(Trap::TypeMismatch(Opcode::Add)),
    };
    let layout = module
        .type_field_offsets(owner)
        .filter(|layout| layout.slot_count() == fields.len())
        .ok_or_else(|| {
            Trap::FieldPointerWalk(Cow::Owned(alloc::format!(
                "pointer arithmetic on the address of a field of `{}`: no byte layout of its \
                 fields is recorded, so they cannot be walked by a pointer",
                module.type_full_name(owner).unwrap_or("<unknown>")
            )))
        })?;
    let start = layout
        .offset_of(slot as usize)
        .ok_or(Trap::TypeMismatch(Opcode::Add))?;
    Ok((
        FieldContainer {
            object,
            owner,
            layout,
            fields,
        },
        start,
    ))
}

/// Whether a managed pointer is one of the byte-addressed kinds that pointer arithmetic
/// walks: a `localloc` (`stackalloc`) buffer pointer, a pinned-array element pointer, a
/// pinned-string character pointer, or the address of a frame local/argument or a static
/// (which arithmetic promotes to its `*Bytes` displaced form). A heap field's or a box's
/// pointer is not byte-addressed here.
fn is_raw_pointer(location: &Location) -> bool {
    matches!(
        location,
        Location::Stack { .. }
            | Location::Element { .. }
            | Location::StringChar { .. }
            | Location::Local { .. }
            | Location::Arg { .. }
            | Location::LocalBytes { .. }
            | Location::ArgBytes { .. }
            | Location::Static { .. }
            | Location::StaticBytes { .. }
    )
}

/// The signed BYTE difference `a - b` of two raw pointers into the SAME buffer/array/slot,
/// for `pointer - pointer` (III.1.5). `None` for pointers into different allocations
/// -- subtracting those is undefined, and returning `None` lets
/// the op trap, as in .NET.
fn raw_pointer_byte_difference(heap: &Heap, a: &Location, b: &Location) -> Option<i64> {
    if let (Some((base_a, off_a)), Some((base_b, off_b))) = (slot_pointer(a), slot_pointer(b)) {
        return (base_a == base_b).then(|| i64::from(off_a) - i64::from(off_b));
    }
    if let (Some((slot_a, off_a)), Some((slot_b, off_b))) = (static_pointer(a), static_pointer(b)) {
        return (slot_a == slot_b).then(|| element_displacement(off_a) - element_displacement(off_b));
    }
    if let (Some((base_a, off_a)), Some((base_b, off_b))) =
        (array_pointer(heap, a), array_pointer(heap, b))
    {
        return (base_a == base_b).then(|| (off_a as i64).wrapping_sub(off_b as i64));
    }
    match (a, b) {
        (
            Location::Stack {
                frame: fa,
                buffer: ba,
                offset: oa,
            },
            Location::Stack {
                frame: fb,
                buffer: bb,
                offset: ob,
            },
        ) if fa == fb && ba == bb => Some(i64::from(*oa) - i64::from(*ob)),
        (
            Location::Element {
                array: aa,
                index: ia,
                byte_offset: oa,
            },
            Location::Element {
                array: ab,
                index: ib,
                byte_offset: ob,
            },
        ) if aa == ab && ia == ib => Some(i64::from(*oa) - i64::from(*ob)),
        (
            Location::StringChar {
                string: sa,
                byte_offset: oa,
            },
            Location::StringChar {
                string: sb,
                byte_offset: ob,
            },
        ) if sa == sb => Some(i64::from(*oa) - i64::from(*ob)),
        _ => None,
    }
}

/// A stable TOTAL order over managed pointers for the unsafe comparison operators
/// (III.1.5): same-base slot pointers compare by byte displacement (a typed slot address
/// and its displaced form normalize to one base, so `&s == &s + 0` and `&s + 8 > &s`
/// hold); the raw buffer/element kinds compare within their base; pointers into
/// different bases get an arbitrary but stable order (the CLI leaves cross-allocation
/// comparison unspecified -- unsafe code may still form it, so it must not trap).
fn pointer_compare(heap: &Heap, a: &Location, b: &Location) -> core::cmp::Ordering {
    if let (Some((base_a, off_a)), Some((base_b, off_b))) = (slot_pointer(a), slot_pointer(b)) {
        return base_a.cmp(&base_b).then(off_a.cmp(&off_b));
    }
    if let (Some((slot_a, off_a)), Some((slot_b, off_b))) = (static_pointer(a), static_pointer(b)) {
        return slot_a.cmp(&slot_b).then(element_displacement(off_a).cmp(&element_displacement(off_b)));
    }
    if let (Some(a), Some(b)) = (array_pointer(heap, a), array_pointer(heap, b)) {
        return a.cmp(&b);
    }
    fn rank(location: &Location) -> u8 {
        match location {
            Location::Local { .. } | Location::LocalBytes { .. } => 0,
            Location::Arg { .. } | Location::ArgBytes { .. } => 1,
            Location::Stack { .. } => 2,
            Location::Element { .. } => 3,
            Location::Field { .. } => 4,
            Location::Static { .. } | Location::StaticBytes { .. } => 5,
            Location::Boxed { .. } => 6,
            Location::Nested { .. } => 7,
            Location::StringChar { .. } => 8,
        }
    }
    match (a, b) {
        (
            Location::Stack {
                frame: fa,
                buffer: ba,
                offset: oa,
            },
            Location::Stack {
                frame: fb,
                buffer: bb,
                offset: ob,
            },
        ) => (fa, ba, oa).cmp(&(fb, bb, ob)),
        (
            Location::Element {
                array: aa,
                index: ia,
                byte_offset: oa,
            },
            Location::Element {
                array: ab,
                index: ib,
                byte_offset: ob,
            },
        ) => (aa, ia, oa).cmp(&(ab, ib, ob)),
        (
            Location::Field {
                object: oa,
                slot: sa,
            },
            Location::Field {
                object: ob,
                slot: sb,
            },
        ) => (oa, sa).cmp(&(ob, sb)),
        (Location::Static { slot: sa }, Location::Static { slot: sb }) => sa.cmp(sb),
        (Location::Boxed { object: oa }, Location::Boxed { object: ob }) => oa.cmp(ob),
        (
            Location::StringChar {
                string: sa,
                byte_offset: oa,
            },
            Location::StringChar {
                string: sb,
                byte_offset: ob,
            },
        ) => (sa, oa).cmp(&(sb, ob)),
        (
            Location::Nested {
                base: ba,
                slot: sa,
            },
            Location::Nested {
                base: bb,
                slot: sb,
            },
        ) => pointer_compare(heap, ba, bb).then(sa.cmp(sb)),
        _ => rank(a).cmp(&rank(b)),
    }
}

/// An array pointer's byte displacement from the start of its array. The element
/// index and an arithmetic offset, forward or back ([`element_displacement`]), can name
/// the same address in different ways.
fn array_pointer(heap: &Heap, location: &Location) -> Option<(ObjectRef, u64)> {
    let Location::Element {
        array,
        index,
        byte_offset,
    } = location else {
        return None;
    };
    let width = heap.array_element_width(*array)? as u64;
    let offset = (*index as u64).wrapping_mul(width).wrapping_add(element_displacement(*byte_offset) as u64);
    Some((*array, offset))
}

/// Normalizes a frame-slot pointer to its `(base, byte displacement)` form: a plain
/// local/arg address IS its slot at displacement 0, and the `*Bytes` forms carry the
/// displacement pointer arithmetic accumulated. `None` for the non-slot kinds. The base
/// distinguishes locals from arguments so `&local` never aliases `&arg`.
fn slot_pointer(location: &Location) -> Option<((bool, usize, usize), u32)> {
    match location {
        Location::Local { frame, slot } => Some(((true, *frame, *slot), 0)),
        Location::LocalBytes {
            frame,
            slot,
            byte_offset,
        } => Some(((true, *frame, *slot), *byte_offset)),
        Location::Arg { frame, slot } => Some(((false, *frame, *slot), 0)),
        Location::ArgBytes {
            frame,
            slot,
            byte_offset,
        } => Some(((false, *frame, *slot), *byte_offset)),
        _ => None,
    }
}

/// The storage a pointer is into, as `(kind, a, b)`: a frame's local or argument, a frame's `localloc`
/// buffer, a heap object (an array, a string, a class instance or a box) or a static. A field's
/// address ([`Location::Nested`]) is into its container's storage.
fn pointer_storage(location: &Location) -> (u8, usize, usize) {
    match location {
        Location::Local { frame, slot } | Location::LocalBytes { frame, slot, .. } => (0, *frame, *slot),
        Location::Arg { frame, slot } | Location::ArgBytes { frame, slot, .. } => (1, *frame, *slot),
        Location::Stack { frame, buffer, .. } => (2, *frame, *buffer),
        Location::Element { array: object, .. }
        | Location::StringChar { string: object, .. }
        | Location::Field { object, .. }
        | Location::Boxed { object } => (3, object.index() as usize, 0),
        Location::Static { slot } | Location::StaticBytes { slot, .. } => (4, *slot, 0),
        Location::Nested { base, .. } => pointer_storage(base),
    }
}

/// Whether the pointers `a` and `b` name one place, however each was reached: the same location; one
/// element of one array, given by an index and a byte displacement two ways (ECMA-335 6th ed I.8.9.1:
/// the elements lie at their stride); a variable's or a static's start, given plainly and as its first
/// byte; or one field of one such place.
fn same_place(heap: &Heap, a: &Location, b: &Location) -> bool {
    let plain = |location: &Location| match *location {
        Location::LocalBytes { frame, slot, byte_offset: 0 } => Location::Local { frame, slot },
        Location::ArgBytes { frame, slot, byte_offset: 0 } => Location::Arg { frame, slot },
        Location::StaticBytes { slot, byte_offset: 0 } => Location::Static { slot },
        ref other => other.clone(),
    };
    match (plain(a), plain(b)) {
        (a, b) if a == b => true,
        (
            Location::Element { array: x, index: i, byte_offset: o },
            Location::Element { array: y, index: j, byte_offset: p },
        ) if x == y => heap
            .array_declared_element_width(x)
            .and_then(|stride| Some((element_byte_position(i, o, stride)?, element_byte_position(j, p, stride)?)))
            .is_some_and(|(m, n)| m == n),
        (Location::Nested { base: x, slot: s }, Location::Nested { base: y, slot: t }) => s == t && same_place(heap, &x, &y),
        _ => false,
    }
}

/// The byte offset within its container of the field whose address `location` is ([`Location::Nested`]),
/// by the layout the loader recorded for the container's declared struct type ([`declared_storage_at`]).
/// `None` for any other pointer and where no layout was recorded: the loader records none for an
/// explicit layout or a `[StructLayout]` packing or size, whose fields' order does not say where they sit.
fn recorded_field_offset(
    method_of: &impl Fn(usize) -> Option<MethodId>, vm: &Vm, module: &Module, location: &Location,
) -> Option<u32> {
    let Location::Nested { base, slot } = location else {
        return None;
    };
    let crate::module::FieldStorage::ValueType(owner) = declared_storage_at(method_of, vm, module, base)? else {
        return None;
    };
    let offsets = module.type_field_offsets(owner)?;
    let slot = *slot as usize;
    if slot < offsets.slot_count() { offsets.offset_of(slot) } else { None }
}

/// [`recorded_field_offset`] in the frame at `frame_index`, which runs `method` above `callers`.
fn field_offset_in<'a>(
    module: Option<&'a Module>, vm: &'a Vm, method: MethodId, frame_index: usize, callers: &'a [Frame],
) -> impl Fn(&Location) -> Option<u32> + 'a {
    move |location| {
        let method_of = |index: usize| if index == frame_index { Some(method) } else { callers.get(index).map(|frame| frame.method) };
        recorded_field_offset(&method_of, vm, module?, location)
    }
}

/// A static's pointer as `(slot, byte displacement)`: the static's own address at displacement 0,
/// and [`Location::StaticBytes`] with the displacement pointer arithmetic accumulated. `None` for any
/// other kind.
fn static_pointer(location: &Location) -> Option<(usize, u32)> {
    match location {
        Location::Static { slot } => Some((*slot, 0)),
        Location::StaticBytes { slot, byte_offset } => Some((*slot, *byte_offset)),
        _ => None,
    }
}

/// The integer-only binary operations, computed at the result category's width.
fn binary_integer(opcode: Opcode, a: Value, b: Value) -> Result<Value, Trap> {
    let (av, ak) = int_parts(a).ok_or(Trap::TypeMismatch(opcode))?;
    let (bv, bk) = int_parts(b).ok_or(Trap::TypeMismatch(opcode))?;
    let kind = combine(ak, bk).ok_or(Trap::TypeMismatch(opcode))?;
    let result = if kind == IntKind::I32 {
        i64::from(integer_op_32(opcode, av as i32, bv as i32)?)
    } else {
        integer_op_64(opcode, av, bv)?
    };
    Ok(wrap_int(kind, result))
}

fn integer_op_32(opcode: Opcode, x: i32, y: i32) -> Result<i32, Trap> {
    Ok(match opcode {
        Opcode::Add => x.wrapping_add(y),
        Opcode::Sub => x.wrapping_sub(y),
        Opcode::Mul => x.wrapping_mul(y),
        Opcode::AddOvf => x.checked_add(y).ok_or(Trap::Overflow)?,
        Opcode::SubOvf => x.checked_sub(y).ok_or(Trap::Overflow)?,
        Opcode::MulOvf => x.checked_mul(y).ok_or(Trap::Overflow)?,
        Opcode::AddOvfUn => (x as u32).checked_add(y as u32).ok_or(Trap::Overflow)? as i32,
        Opcode::SubOvfUn => (x as u32).checked_sub(y as u32).ok_or(Trap::Overflow)? as i32,
        Opcode::MulOvfUn => (x as u32).checked_mul(y as u32).ok_or(Trap::Overflow)? as i32,
        Opcode::And => x & y,
        Opcode::Or => x | y,
        Opcode::Xor => x ^ y,
        Opcode::Div => x.checked_div(y).ok_or(Trap::DivideByZero)?,
        Opcode::Rem => x.checked_rem(y).ok_or(Trap::DivideByZero)?,
        Opcode::DivUn => (checked_div_u32(x as u32, y as u32)?) as i32,
        Opcode::RemUn => (checked_rem_u32(x as u32, y as u32)?) as i32,
        _ => return Err(Trap::Unsupported(opcode)),
    })
}

fn integer_op_64(opcode: Opcode, x: i64, y: i64) -> Result<i64, Trap> {
    Ok(match opcode {
        Opcode::Add => x.wrapping_add(y),
        Opcode::Sub => x.wrapping_sub(y),
        Opcode::Mul => x.wrapping_mul(y),
        Opcode::AddOvf => x.checked_add(y).ok_or(Trap::Overflow)?,
        Opcode::SubOvf => x.checked_sub(y).ok_or(Trap::Overflow)?,
        Opcode::MulOvf => x.checked_mul(y).ok_or(Trap::Overflow)?,
        Opcode::AddOvfUn => (x as u64).checked_add(y as u64).ok_or(Trap::Overflow)? as i64,
        Opcode::SubOvfUn => (x as u64).checked_sub(y as u64).ok_or(Trap::Overflow)? as i64,
        Opcode::MulOvfUn => (x as u64).checked_mul(y as u64).ok_or(Trap::Overflow)? as i64,
        Opcode::And => x & y,
        Opcode::Or => x | y,
        Opcode::Xor => x ^ y,
        Opcode::Div => x.checked_div(y).ok_or(Trap::DivideByZero)?,
        Opcode::Rem => x.checked_rem(y).ok_or(Trap::DivideByZero)?,
        Opcode::DivUn => (checked_div_u64(x as u64, y as u64)?) as i64,
        Opcode::RemUn => (checked_rem_u64(x as u64, y as u64)?) as i64,
        _ => return Err(Trap::Unsupported(opcode)),
    })
}

fn checked_div_u32(x: u32, y: u32) -> Result<u32, Trap> {
    x.checked_div(y).ok_or(Trap::DivideByZero)
}

fn checked_rem_u32(x: u32, y: u32) -> Result<u32, Trap> {
    x.checked_rem(y).ok_or(Trap::DivideByZero)
}

fn checked_div_u64(x: u64, y: u64) -> Result<u64, Trap> {
    x.checked_div(y).ok_or(Trap::DivideByZero)
}

fn checked_rem_u64(x: u64, y: u64) -> Result<u64, Trap> {
    x.checked_rem(y).ok_or(Trap::DivideByZero)
}

/// `shl`, `shr`, `shr.un`: the result has the first operand's category; the
/// second operand is the shift amount (III.1.5, Table 15).
fn shift(opcode: Opcode, value: Value, amount: Value) -> Result<Value, Trap> {
    let (raw, kind) = int_parts(value).ok_or(Trap::TypeMismatch(opcode))?;
    let (shift_by, _) = int_parts(amount).ok_or(Trap::TypeMismatch(opcode))?;
    let shift_by = shift_by as u32;
    let result = match kind {
        IntKind::I32 => {
            let x = raw as i32;
            let shifted = match opcode {
                Opcode::Shl => x.wrapping_shl(shift_by),
                Opcode::Shr => x.wrapping_shr(shift_by),
                Opcode::ShrUn => (x as u32).wrapping_shr(shift_by) as i32,
                _ => return Err(Trap::Unsupported(opcode)),
            };
            i64::from(shifted)
        }
        _ => match opcode {
            Opcode::Shl => raw.wrapping_shl(shift_by),
            Opcode::Shr => raw.wrapping_shr(shift_by),
            Opcode::ShrUn => (raw as u64).wrapping_shr(shift_by) as i64,
            _ => return Err(Trap::Unsupported(opcode)),
        },
    };
    Ok(wrap_int(kind, result))
}

fn negate(value: Value) -> Result<Value, Trap> {
    match value {
        Value::Int32(x) => Ok(Value::Int32(x.wrapping_neg())),
        Value::Int64(x) => Ok(Value::Int64(x.wrapping_neg())),
        Value::NativeInt(x) => Ok(Value::NativeInt(x.wrapping_neg())),
        #[cfg(feature = "float")]
        Value::Float(x) => Ok(Value::Float(-x)),
        #[cfg(feature = "float")]
        Value::Single(x) => Ok(Value::Single(-x)),
        Value::Object(_) | Value::Null | Value::Struct(_) | Value::ByRef(_) => {
            Err(Trap::TypeMismatch(Opcode::Neg))
        }
        #[cfg(feature = "typed-references")]
        Value::TypedRef { .. } => Err(Trap::TypeMismatch(Opcode::Neg)),
    }
}

fn bitwise_not(value: Value) -> Result<Value, Trap> {
    match value {
        Value::Int32(x) => Ok(Value::Int32(!x)),
        Value::Int64(x) => Ok(Value::Int64(!x)),
        Value::NativeInt(x) => Ok(Value::NativeInt(!x)),
        _ => Err(Trap::TypeMismatch(Opcode::Not)),
    }
}

/// Converts the top-of-stack value per a `conv.*` opcode (III.3.27). Float to
/// integer truncates toward zero; integer narrowing truncates the high bits;
/// `conv.i*` widen by sign-extension and `conv.u*` by zero-extension; results
/// narrower than `int32` fill the slot. `conv.r.un` reads the source as unsigned.
fn convert(opcode: Opcode, value: Value) -> Result<Value, Trap> {
    #[cfg(feature = "float")]
    if opcode == Opcode::ConvRUn {
        let unsigned = match value {
            Value::Int32(x) => u64::from(x as u32),
            Value::Int64(x) | Value::NativeInt(x) => x as u64,
            _ => return Err(Trap::TypeMismatch(opcode)),
        };
        return Ok(Value::Float(unsigned as f64));
    }

    #[cfg(feature = "float")]
    if matches!(opcode, Opcode::ConvR4 | Opcode::ConvR8) {
        let float = match value {
            Value::Int32(x) => f64::from(x),
            Value::Int64(x) | Value::NativeInt(x) => x as f64,
            Value::Float(f) => f,
            Value::Single(f) => f64::from(f),
            _ => return Err(Trap::TypeMismatch(opcode)),
        };
        return Ok(if opcode == Opcode::ConvR4 {
            Value::Single(float as f32)
        } else {
            Value::Float(float)
        });
    }


    let (source, from_32) = match value {
        Value::Int32(x) => (i64::from(x), true),
        Value::Int64(x) => (x, false),
        Value::NativeInt(x) => (x, false),
        #[cfg(feature = "float")]
        Value::Float(f) => (float_to_intermediate(opcode, f), false),
        #[cfg(feature = "float")]
        Value::Single(f) => (float_to_intermediate(opcode, f64::from(f)), false),
        Value::Object(_) | Value::Null | Value::Struct(_) | Value::ByRef(_) => {
            return Err(Trap::TypeMismatch(opcode));
        }
        #[cfg(feature = "typed-references")]
        Value::TypedRef { .. } => return Err(Trap::TypeMismatch(opcode)),
    };
    let zero_extended = if from_32 {
        i64::from(source as u32)
    } else {
        source
    };
    Ok(match opcode {
        Opcode::ConvI1 => Value::Int32(i32::from(source as i8)),
        Opcode::ConvU1 => Value::Int32(i32::from(source as u8)),
        Opcode::ConvI2 => Value::Int32(i32::from(source as i16)),
        Opcode::ConvU2 => Value::Int32(i32::from(source as u16)),
        Opcode::ConvI4 => Value::Int32(source as i32),
        Opcode::ConvU4 => Value::Int32(source as u32 as i32),
        Opcode::ConvI8 => Value::Int64(source),
        Opcode::ConvU8 => Value::Int64(zero_extended),
        Opcode::ConvI => Value::NativeInt(source),
        Opcode::ConvU => Value::NativeInt(zero_extended),
        _ => return Err(Trap::Unsupported(opcode)),
    })
}

/// A float converted to the intermediate a `conv.*` truncates from, answering what .NET 9 and later
/// answer when the value does not fit or is NaN: the conversion SATURATES at the target's range, and
/// NaN converts to 0.
///
/// ECMA-335 III.3.27 declines to choose here: "If overflow occurs converting a floating-point type
/// to an integer, or if the floating-point value being converted to an integer is a NaN, the value
/// returned is unspecified." Where the standard leaves a value unspecified, Lamella answers as
/// modern .NET does, and from .NET 9 these conversions saturate on every platform.
///
/// The narrow targets (`conv.i1`, `u1`, `i2`, `u2`) saturate at `int`'s range and are then truncated,
/// as .NET 9 does it: `(byte)1e10` is 255 and `(short)1e10` is -1, the low bits of `int.MaxValue`,
/// not the narrow type's own maximum.
#[cfg(feature = "float")]
fn float_to_intermediate(opcode: Opcode, float: f64) -> i64 {
    match opcode {
        Opcode::ConvU8 | Opcode::ConvU => float as u64 as i64,
        Opcode::ConvI8 | Opcode::ConvI => float as i64,
        Opcode::ConvU4 => i64::from(float as u32),
        _ => i64::from(float as i32),
    }
}

/// The checked conversions `conv.ovf.*`: like [`convert`] but yielding [`Trap::Overflow`]
/// (the `OverflowException` site) when the source does not fit the target type. The `.un`
/// forms read the source as unsigned.
fn convert_checked(opcode: Opcode, value: Value) -> Result<Value, Trap> {
    let unsigned_source = matches!(
        opcode,
        Opcode::ConvOvfI1Un
            | Opcode::ConvOvfI2Un
            | Opcode::ConvOvfI4Un
            | Opcode::ConvOvfI8Un
            | Opcode::ConvOvfU1Un
            | Opcode::ConvOvfU2Un
            | Opcode::ConvOvfU4Un
            | Opcode::ConvOvfU8Un
            | Opcode::ConvOvfIUn
            | Opcode::ConvOvfUUn
    );
    let source: i128 = match value {
        Value::Int32(x) if unsigned_source => i128::from(x as u32),
        Value::Int32(x) => i128::from(x),
        Value::Int64(x) | Value::NativeInt(x) if unsigned_source => i128::from(x as u64),
        Value::Int64(x) | Value::NativeInt(x) => i128::from(x),
        #[cfg(feature = "float")]
        Value::Float(f) if f.is_nan() || f.is_infinite() => return Err(Trap::Overflow),
        #[cfg(feature = "float")]
        Value::Float(f) => f as i128,
        #[cfg(feature = "float")]
        Value::Single(f) if f.is_nan() || f.is_infinite() => return Err(Trap::Overflow),
        #[cfg(feature = "float")]
        Value::Single(f) => f as i128,
        _ => return Err(Trap::TypeMismatch(opcode)),
    };
    let (min, max): (i128, i128) = match opcode {
        Opcode::ConvOvfI1 | Opcode::ConvOvfI1Un => (i128::from(i8::MIN), i128::from(i8::MAX)),
        Opcode::ConvOvfU1 | Opcode::ConvOvfU1Un => (0, i128::from(u8::MAX)),
        Opcode::ConvOvfI2 | Opcode::ConvOvfI2Un => (i128::from(i16::MIN), i128::from(i16::MAX)),
        Opcode::ConvOvfU2 | Opcode::ConvOvfU2Un => (0, i128::from(u16::MAX)),
        Opcode::ConvOvfI4 | Opcode::ConvOvfI4Un => (i128::from(i32::MIN), i128::from(i32::MAX)),
        Opcode::ConvOvfU4 | Opcode::ConvOvfU4Un => (0, i128::from(u32::MAX)),
        Opcode::ConvOvfI8 | Opcode::ConvOvfI8Un | Opcode::ConvOvfI | Opcode::ConvOvfIUn => {
            (i128::from(i64::MIN), i128::from(i64::MAX))
        }
        Opcode::ConvOvfU8 | Opcode::ConvOvfU8Un | Opcode::ConvOvfU | Opcode::ConvOvfUUn => {
            (0, i128::from(u64::MAX))
        }
        _ => return Err(Trap::Unsupported(opcode)),
    };
    if source < min || source > max {
        return Err(Trap::Overflow);
    }
    Ok(match opcode {
        Opcode::ConvOvfI8 | Opcode::ConvOvfI8Un | Opcode::ConvOvfU8 | Opcode::ConvOvfU8Un => {
            Value::Int64(source as i64)
        }
        Opcode::ConvOvfI | Opcode::ConvOvfIUn | Opcode::ConvOvfU | Opcode::ConvOvfUUn => {
            Value::NativeInt(source as i64)
        }
        _ => Value::Int32(source as i32),
    })
}

/// The relation a comparison or conditional branch tests.
#[derive(Clone, Copy)]
enum Relation {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

/// Decodes a comparison or conditional-branch opcode into its relation and
/// whether it is the unsigned/unordered variant.
fn relation_of(opcode: Opcode) -> Option<(Relation, bool)> {
    Some(match opcode {
        Opcode::Ceq | Opcode::Beq | Opcode::BeqS => (Relation::Equal, false),
        Opcode::BneUn | Opcode::BneUnS => (Relation::NotEqual, true),
        Opcode::Cgt | Opcode::Bgt | Opcode::BgtS => (Relation::Greater, false),
        Opcode::CgtUn | Opcode::BgtUn | Opcode::BgtUnS => (Relation::Greater, true),
        Opcode::Clt | Opcode::Blt | Opcode::BltS => (Relation::Less, false),
        Opcode::CltUn | Opcode::BltUn | Opcode::BltUnS => (Relation::Less, true),
        Opcode::Bge | Opcode::BgeS => (Relation::GreaterOrEqual, false),
        Opcode::BgeUn | Opcode::BgeUnS => (Relation::GreaterOrEqual, true),
        Opcode::Ble | Opcode::BleS => (Relation::LessOrEqual, false),
        Opcode::BleUn | Opcode::BleUnS => (Relation::LessOrEqual, true),
        _ => return None,
    })
}

/// Evaluates a comparison or conditional branch (III.1.5, Table 13). Integers
/// compare signed unless the unsigned variant is used; floats compare ordered,
/// with the unordered (NaN) result chosen by the `.un` variant. `field_offset` gives a
/// field's address its recorded offset in its container ([`recorded_field_offset`]).
fn compare(heap: &Heap, field_offset: &dyn Fn(&Location) -> Option<u32>, opcode: Opcode, a: Value, b: Value) -> Result<bool, Trap> {
    let (relation, unordered_or_unsigned) = relation_of(opcode).ok_or(Trap::Unsupported(opcode))?;
    #[cfg(feature = "float")]
    if let (Some(x), Some(y)) = (as_double(&a), as_double(&b)) {
        if x.is_nan() || y.is_nan() {
            return Ok(unordered_or_unsigned && !matches!(relation, Relation::Equal));
        }
        return Ok(apply_relation(relation, x.partial_cmp(&y)));
    }
    if let (Value::ByRef(la), Value::ByRef(lb)) = (&a, &b) {
        let field = |location: &Location| matches!(location, Location::Nested { .. });
        if let (Location::Nested { base: ba, slot: sa }, Location::Nested { base: bb, slot: sb }) = (la, lb)
            && same_place(heap, ba, bb)
        {
            let ordering = match (field_offset(la), field_offset(lb)) {
                (Some(x), Some(y)) => x.cmp(&y),
                _ if sa == sb => core::cmp::Ordering::Equal,
                _ => return Err(Trap::Unsupported(opcode)),
            };
            return Ok(apply_relation(relation, Some(ordering)));
        }
        if (field(la) || field(lb)) && pointer_storage(la) == pointer_storage(lb) {
            return Err(Trap::Unsupported(opcode));
        }
        let ordering = pointer_compare(heap, la, lb);
        return Ok(apply_relation(relation, Some(ordering)));
    }
    if let (Value::ByRef(_), other) | (other, Value::ByRef(_)) = (&a, &b) {
        if pointer_offset(other) == Some(0) {
            return match (relation, unordered_or_unsigned) {
                (Relation::Equal, _) => Ok(false),
                (Relation::NotEqual, _) | (Relation::Greater, true) => Ok(true),
                _ => Err(Trap::TypeMismatch(opcode)),
            };
        }
    }
    if matches!(a, Value::Object(_) | Value::Null) || matches!(b, Value::Object(_) | Value::Null) {
        let equal = reference_equal(a, b);
        return match (relation, unordered_or_unsigned) {
            (Relation::Equal, _) => Ok(equal),
            (Relation::NotEqual, _) | (Relation::Greater, true) => Ok(!equal),
            _ => Err(Trap::TypeMismatch(opcode)),
        };
    }
    let (av, ak) = int_parts(a).ok_or(Trap::TypeMismatch(opcode))?;
    let (bv, bk) = int_parts(b).ok_or(Trap::TypeMismatch(opcode))?;
    let _ = (ak, bk);
    let ordering = if unordered_or_unsigned {
        (av as u64).cmp(&(bv as u64))
    } else {
        av.cmp(&bv)
    };
    Ok(apply_relation(relation, Some(ordering)))
}

fn apply_relation(relation: Relation, ordering: Option<core::cmp::Ordering>) -> bool {
    use core::cmp::Ordering::{Equal, Greater, Less};
    let Some(ordering) = ordering else {
        return false;
    };
    match relation {
        Relation::Equal => ordering == Equal,
        Relation::NotEqual => ordering != Equal,
        Relation::Less => ordering == Less,
        Relation::LessOrEqual => ordering != Greater,
        Relation::Greater => ordering == Greater,
        Relation::GreaterOrEqual => ordering != Less,
    }
}

fn branch_target(instruction: &Instruction, code_len: usize) -> Result<usize, Trap> {
    let target = match instruction.operand {
        Operand::Target(index) => index,
        _ => return Err(Trap::MalformedInstruction(instruction.opcode)),
    };
    if target as usize >= code_len {
        return Err(Trap::BranchOutOfRange(target));
    }
    Ok(target as usize)
}

fn int8_operand(instruction: &Instruction) -> Result<i8, Trap> {
    match instruction.operand {
        Operand::Int8(value) => Ok(value),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

fn int32_operand(instruction: &Instruction) -> Result<i32, Trap> {
    match instruction.operand {
        Operand::Int32(value) => Ok(value),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

fn int64_operand(instruction: &Instruction) -> Result<i64, Trap> {
    match instruction.operand {
        Operand::Int64(value) => Ok(value),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

#[cfg(feature = "float")]
fn float32_operand(instruction: &Instruction) -> Result<f32, Trap> {
    match instruction.operand {
        Operand::Float32(value) => Ok(value),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

#[cfg(feature = "float")]
fn float64_operand(instruction: &Instruction) -> Result<f64, Trap> {
    match instruction.operand {
        Operand::Float64(value) => Ok(value),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

fn var_operand(instruction: &Instruction) -> Result<u16, Trap> {
    match instruction.operand {
        Operand::Variable(slot) => Ok(slot),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

fn token_operand(instruction: &Instruction) -> Result<Token, Trap> {
    match instruction.operand {
        Operand::Token(token) => Ok(token),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

/// The object reference a field or instance instruction expects on the stack: an
/// object, [`Trap::NullReference`] for null, or [`Trap::TypeMismatch`] otherwise.
fn object_ref(value: Value, opcode: Opcode) -> Result<ObjectRef, Trap> {
    match value {
        Value::Object(reference) => Ok(reference),
        Value::Null => Err(Trap::NullReference),
        _ => Err(Trap::TypeMismatch(opcode)),
    }
}

/// The sentinel `ldvirtftn` pushes for `d.Invoke` on a delegate value (the `new D(d)`
/// delegate-from-a-delegate-value form): the delegate `newobj` recognizes it and copies `d`'s
/// invocation list instead of binding the bodyless Invoke. `u32::MAX` is never a real method id --
/// ids are assigned sequentially from 0, so it cannot collide.
const DELEGATE_INVOKE_FPTR: MethodId = u32::MAX;

/// Extracts a method id from a function pointer: `ldftn` / `ldvirtftn` push the method
/// id as a native int, and a delegate constructor consumes it.
fn function_pointer(value: Value) -> Result<MethodId, Trap> {
    match value {
        Value::NativeInt(method) => Ok(method as MethodId),
        _ => Err(Trap::TypeMismatch(Opcode::Newobj)),
    }
}

/// The runtime [`crate::module::TypeId`] a `callvirt` / `ldvirtftn` dispatches on for an
/// object receiver.
///
/// An object that carries its own identity answers from it: a field-carrying instance's type id,
/// a delegate's recorded delegate type, a boxed value type's declared type (resolved from the
/// box's tag, so an interface `callvirt` on a boxed struct reaches the struct's implementation).
/// An object this runtime represents STRUCTURALLY -- a heap string, a vector, a rectangular array
/// -- has no per-object identity and answers from the [`IntrinsicType`] its shape names, through
/// the one facility both loaders record.
///
///
/// `None` now means only that no loader recorded a type for this shape, which leaves the caller's
/// static target in place rather than dispatching against something invented.
pub(crate) fn receiver_type_id(module: &Module, vm: &Vm, this: ObjectRef) -> Option<u32> {
    vm.heap()
        .type_of(this)
        .or_else(|| vm.heap().delegate_type_id(this))
        .or_else(|| {
            vm.heap()
                .boxed_type_token(this)
                .and_then(|token| module.type_id_by_handle(token))
        })
        .or_else(|| {
            vm.heap()
                .structural_type(this)
                .and_then(|which| module.intrinsic_type_id(which))
        })
}

/// Resolves a `callvirt` target on a `this` of `runtime_type`: an explicit interface
/// implementation (`MethodImpl`) wins outright -- it is the only way to reach a private,
/// interface-named body and to tell two same-signature interface methods apart -- then a
/// class virtual via the runtime type's vtable slot, then a target PROVEN non-virtual (which
/// binds statically, III.4.2), else an interface/abstract method by signature key, else the
/// static target (a string/array `this`, or a non-virtual target the maps could not prove).
pub(crate) fn resolve_callvirt(
    module: &Module,
    static_method: Option<MethodId>,
    sig_key: Option<u32>,
    runtime_type: Option<u32>,
    explicit_override: Option<MethodId>,
) -> Option<MethodId> {
    if explicit_override.is_some() {
        return explicit_override;
    }
    if let Some(method) = static_method {
        if let Some(slot) = module.method_slot(method) {
            return Some(
                runtime_type
                    .and_then(|type_id| module.vtable_entry(type_id, slot))
                    .unwrap_or(method),
            );
        }
        if let (Some(key), Some(declaring)) = (sig_key, module.method_type(method)) {
            if module.sig_dispatch_nonvirtual(declaring, key) == Some(method) {
                return Some(method);
            }
        }
    }
    if let (Some(key), Some(type_id)) = (sig_key, runtime_type) {
        if let Some(method) = module.sig_dispatch(type_id, key) {
            return Some(method);
        }
        let mut ancestor = type_id;
        for _ in 0..module.type_count() {
            match module.type_base(ancestor) {
                Some(base) => {
                    if let Some(method) = module.sig_dispatch(base, key) {
                        return Some(method);
                    }
                    ancestor = base;
                }
                None => break,
            }
        }
        if static_method.is_none() {
            if let Some(method) = module.sig_dispatch_nonvirtual(type_id, key) {
                return Some(method);
            }
        }
    }
    static_method
}

/// Gives a value type's OWN instance method, dispatched on a boxed receiver, the `this` it takes:
/// a managed pointer INTO the box (ECMA-335 III.4.2 -- the unbox the JIT inserts for a box
/// dispatched to its value type's method), so the body reads the boxed value through `this`
/// (`ldarg.0; ldind.*` / `ldfld`). Only a value-type target on an actual box changes;
/// reference-type dispatch keeps its object `this`.
///
/// Both dispatchers apply it: `callvirt`, and `MethodInfo.Invoke`, which dispatches the way a
/// `callvirt` naming its method would.
pub(crate) fn value_type_receiver(module: &Module, vm: &Vm, method: MethodId, args: &mut [Value]) {
    if module.method_declares_value_type(method) {
        if let Some(&Value::Object(reference)) = args.first() {
            if vm.heap().boxed_type_token(reference).is_some() {
                args[0] = Value::ByRef(Location::Boxed { object: reference });
            }
        }
    }
}

/// The zero of the type `type_id` names, whose asm-folded handle is `handle`: what `initobj` writes,
/// and what the box of a value type holds when no constructor has run. A primitive answers its zero
/// at its width, an enum its underlying zero, a struct its zeroed fields, and a reference type null.
pub(crate) fn zero_of_type(module: &Module, type_id: Option<TypeId>, handle: u64) -> Value {
    if names_a_reference_type(module, type_id, handle) {
        return Value::Null;
    }
    let defaults = type_id.and_then(|type_id| module.type_field_defaults(type_id));
    if defaults.as_ref().is_none_or(|fields| fields.is_empty()) && !module.is_enum_by_handle(handle)
    {
        if let Some(zero) = type_id.and_then(|id| module.primitive_zero_of_type(id)) {
            return zero;
        }
        if type_id.is_some_and(|type_id| !module.type_is_value_type(type_id)) {
            return Value::Null;
        }
    }
    match defaults {
        Some(defaults) if module.is_enum_by_handle(handle) => {
            defaults.into_iter().next().unwrap_or(Value::Int32(0))
        }
        Some(defaults) => Value::Struct(defaults.into_boxed_slice()),
        None => Value::Int32(0),
    }
}

/// Whether the type `type_id` names is a reference type, whose zero is null; where the module has
/// no type id for it, whether the type operand `handle` (asm-folded) names one by its shape.
///
/// With a type id: a type the module records as neither a value type nor an enum, and no primitive,
/// with or without fields. With none: `string`, `object`, an array (no array type has a type id), a
/// vector interface, or a shape the module does not model. Of those shapes, a pointer and a managed
/// pointer are no type argument, so a `default(T)` reaches only the others: a class or interface
/// instantiation, or a multi-dimensional array.
fn names_a_reference_type(module: &Module, type_id: Option<TypeId>, handle: u64) -> bool {
    match type_id {
        Some(type_id) => {
            !module.type_is_value_type(type_id)
                && !module.is_enum_by_handle(handle)
                && module.primitive_zero_of_type(type_id).is_none()
        }
        None => matches!(
            module.cast_elem(handle),
            Some(
                CastElem::String
                    | CastElem::Object
                    | CastElem::Array(_)
                    | CastElem::VectorInterface(_)
                    | CastElem::Lenient
            )
        ),
    }
}

/// The asm-folded handle of `T` when the type `token` names is an instantiation of
/// `System.Nullable<T>`, else `None` -- the one question the four special-cased instruction arms
/// (III.4.1 `box`, III.4.3 `castclass`, III.4.6 `isinst`, III.4.32 / III.4.33 `unbox` /
/// `unbox.any`) ask, in the form all four need the answer in.
///
/// Recorded per TYPE at load rather than per token, so a closed `TypeSpec` in a program and a
/// synthetic token in a rewritten generic body reach the same answer.
#[cfg(feature = "generics")]
pub(crate) fn nullable_underlying_of(module: &Module, asm: u8, token: Token) -> Option<u64> {
    module
        .type_id_of(asm, token)
        .and_then(|type_id| module.nullable_underlying(type_id))
}

/// Always `None` without the monomorphizer, because a `Nullable<T>` is an INSTANTIATION and there
/// is no pass to create one -- so every arm that asks this is unreachable rather than merely idle.
///
/// # Why the gate is one function rather than a `cfg` on each arm
///
/// Four arms ask this question and each keeps its `if let Some(underlying) = ...` unchanged; with
/// the constant `None` the branch folds away and `nullable_from_boxed` loses its only caller, so
/// the whole rule set leaves the build through dead-code elimination rather than through four
/// conditionally-compiled blocks that could drift apart.
#[cfg(not(feature = "generics"))]
pub(crate) fn nullable_underlying_of(_module: &Module, _asm: u8, _token: Token) -> Option<u64> {
    None
}

/// The `Nullable<T>` instance `unbox` / `unbox.any` recover from `source`, or `None` when
/// III.4.32's InvalidCastException applies ("valuetype is a Nullable<T> and obj is not a boxed T").
///
/// `token` names the `Nullable<T>` instantiation and `underlying` is `T`'s handle. A null reference
/// yields the type's own zero -- the same value `initobj` writes, read from the same field defaults,
/// so `(int?)(object)null` and `default(int?)` cannot disagree.
pub(crate) fn nullable_from_boxed(
    module: &Module,
    vm: &Vm,
    asm: u8,
    token: Token,
    underlying: u64,
    source: &Value,
) -> Option<Value> {
    if matches!(source, Value::Null) {
        let defaults = module
            .type_id_of(asm, token)
            .and_then(|type_id| module.type_field_defaults(type_id))?;
        return Some(Value::Struct(defaults.into_boxed_slice()));
    }
    let Value::Object(reference) = source else {
        return None;
    };
    let boxed = vm.heap().boxed_value(*reference)?;
    if !boxed_is_underlying(module, vm.heap().boxed_type_token(*reference)?, underlying) {
        return None;
    }
    Some(Value::Struct(alloc::vec![Value::Int32(1), boxed].into_boxed_slice()))
}

/// Whether a box tagged `box_handle` holds the `underlying` type -- the "obj is a boxed T" test
/// every nullable arm makes.
///
pub(crate) fn boxed_is_underlying(module: &Module, box_handle: u64, underlying: u64) -> bool {
    if box_handle == underlying {
        return true;
    }
    match (
        module.type_id_by_handle(box_handle),
        module.type_id_by_handle(underlying),
    ) {
        (Some(from_box), Some(want)) => from_box == want,
        _ => false,
    }
}

/// Whether `value` can be `castclass`/`isinst` to `token`'s type: null matches; a
/// reference matches when its runtime type is a subtype of the target OR implements the
/// target interface.
///
/// The receiver's heap kind makes the test precise where the target is otherwise
/// unverifiable (an external core type with no module [`crate::module::TypeId`]):
/// - a declared **instance** matches via the subtype relation, or when its runtime type
///   (or a base) implements the target interface; an unresolved non-core target is treated
///   as a match (unverified -- an interface this module cannot see). A **delegate** whose type
///   its `newobj` recorded is tested the same way, as an instance of that type;
/// - a **boxed** value type matches `System.Object`, its own exact value type, a base class of
///   it (`System.ValueType`, and `System.Enum` for an enum), or an interface its value type
///   declares -- so a boxed `int` is precisely *not* a `string` / an unrelated class, but IS a
///   `ValueType` and IS castable to an interface it implements;
/// - a heap **string** matches `System.String`, `System.Object`, or an interface the
///   `System.String` type declares, and nothing else;
/// - a target that is a `Nullable<T>` matches a boxed **T**, per III.4.3 and III.4.6.
pub(crate) fn cast_matches(module: &Module, asm: u8, vm: &Vm, value: &Value, token: Token) -> bool {
    let reference = match value {
        Value::Null => return true,
        Value::Object(reference) => *reference,
        _ => return false,
    };
    if let Some(underlying) = nullable_underlying_of(module, asm, token) {
        return vm
            .heap()
            .boxed_type_token(reference)
            .is_some_and(|box_handle| boxed_is_underlying(module, box_handle, underlying));
    }
    let target_type_id = module.type_id_of(asm, token);
    let declared = vm
        .heap()
        .type_of(reference)
        .or_else(|| vm.heap().delegate_type_id(reference));
    if let Some(runtime) = declared {
        let runtime = if runtime == EXTERNAL_TYPE_ID {
            vm.raised_exception_type(module, reference).unwrap_or(runtime)
        } else {
            runtime
        };
        return match target_type_id {
            Some(target) => {
                module.is_subtype(runtime, target) || module.implements_interface(runtime, target)
            }
            None if module.is_object_type_token(asm, token) => true,
            None if module.is_string_type_token(asm, token) => false,
            None if matches!(module.cast_elem(asm_key(asm, token.0)),
                Some(CastElem::Array(_) | CastElem::MdArray { .. })) => {
                false
            }
            None => true,
        };
    }
    if let Some(box_token) = vm.heap().boxed_type_token(reference) {
        if module.is_object_type_token(asm, token) {
            return true;
        }
        if box_token == asm_key(asm, token.0) {
            return true;
        }
        if let Some(box_type) = module.type_id_by_handle(box_token) {
            if let Some(target) = target_type_id {
                return module.is_subtype(box_type, target)
                    || module.implements_interface(box_type, target);
            }
        }
        return false;
    }
    if vm.heap().is_string(reference) {
        if module.is_string_type_token(asm, token) || module.is_object_type_token(asm, token) {
            return true;
        }
        if let (Some(string_type), Some(target)) =
            (module.intrinsic_type_id(IntrinsicType::String), target_type_id)
        {
            return module.is_subtype(string_type, target)
                || module.implements_interface(string_type, target);
        }
        return false;
    }
    if let Some(op_elem) = vm.heap().array_element_type(reference) {
        let element = (op_elem != 0).then(|| classify_elem_handle(module, op_elem));
        return array_cast_matches(module, asm, element.as_ref(), None, token);
    }
    if let Some((rank, array_type)) = vm.heap().md_array_type(reference) {
        let element = match module.cast_elem(array_type) {
            Some(CastElem::MdArray { element, .. }) => Some(*element),
            _ => None,
        };
        return array_cast_matches(module, asm, element.as_ref(), Some(rank), token);
    }
    if matches!(module.cast_elem(asm_key(asm, token.0)), Some(CastElem::VectorInterface(_))) {
        return false;
    }
    true
}

/// Whether an object reference can be stored in an array element described by `target`.
/// Declared elements use the ordinary cast relation; array elements retain their rank and
/// covariance rules. Shapes without a resolved element identity remain unverified.
pub(crate) fn reference_matches_element(
    module: &Module, vm: &Vm, reference: ObjectRef, target: &CastElem,
) -> bool {
    match target {
        CastElem::Object => true,
        CastElem::String => vm.heap().is_string(reference),
        CastElem::Named(handle) => {
            let asm = (handle >> 32) as u8;
            let token = Token(*handle as u32);
            module.is_object_type_token(asm, token)
                || cast_matches(module, asm, vm, &Value::Object(reference), token)
        }
        CastElem::Array(target_element) => vm.heap().array_element_type(reference)
            .is_some_and(|element| element == 0 || elem_compatible(
                module, &classify_elem_handle(module, element), target_element, ElemRule::Array,
            )),
        CastElem::MdArray { element: target_element, rank: target_rank } => {
            vm.heap().md_array_type(reference).is_some_and(|(rank, array_type)| {
                rank == *target_rank && match module.cast_elem(array_type) {
                    Some(CastElem::MdArray { element, .. }) => {
                        elem_compatible(module, &element, target_element, ElemRule::Array)
                    }
                    _ => true,
                }
            })
        }
        CastElem::Prim(_) | CastElem::GenericValueType => false,
        CastElem::Lenient | CastElem::Pointer(_) | CastElem::Void | CastElem::VectorInterface(_) => true,
    }
}

/// Whether the type operand `token` in assembly `asm` names a VALUE type: `Some(true)` for a
/// primitive or a type the module records as a struct or an enum, `Some(false)` for a reference
/// type -- a class, an interface, `string`, `object` or an array -- and `None` for a type the module
/// cannot resolve or a shape the checks do not model. A class or interface instantiation such as
/// `List<int>` has a lenient cast shape and is answered by the type it resolves to.
fn is_value_type_operand(module: &Module, asm: u8, token: Token) -> Option<bool> {
    match module.cast_elem(asm_key(asm, token.0)) {
        Some(CastElem::Prim(_) | CastElem::GenericValueType) => Some(true),
        Some(
            CastElem::String | CastElem::Object | CastElem::Array(_) | CastElem::MdArray { .. }
            | CastElem::VectorInterface(_),
        ) => Some(false),
        _ => module.type_id_of(asm, token).map(|type_id| module.type_is_value_type(type_id)),
    }
}

/// Whether an array whose `newarr` element handle is `element` (asm-folded) holds exactly the type
/// `token` names in assembly `asm`: the same handle, the same resolved type, or -- where neither
/// side resolves to a type, as for an array, `object` or `string` type argument on a tier with no
/// managed corlib -- the same cast shape.
fn array_element_is(module: &Module, element: u64, asm: u8, token: Token) -> bool {
    let handle = asm_key(asm, token.0);
    if element == handle {
        return true;
    }
    if let (Some(held), Some(wanted)) = (module.type_id_by_handle(element), module.type_id_of(asm, token)) {
        return held == wanted;
    }
    match (module.cast_elem(element), module.cast_elem(handle)) {
        (Some(held), Some(wanted)) => held == wanted && held != CastElem::Lenient,
        _ => false,
    }
}

/// Whether an array casts to the type `token` names. `op_rank` is `None` for a vector and
/// `Some(rank)` for a rectangular array; an untracked allocation has no `op_elem`. Element
/// compatibility follows I.8.7.1 as .NET implements it: same element type, same-width
/// signed/unsigned INTEGER interchange (`int[]` casts to `uint[]`, but `bool`/`char`/floats
/// match only themselves), enum/underlying interchange, and reference-element covariance.
/// The loader's array-base identities apply at every metadata tier: partial managed declarations
/// cannot remove the interfaces built into every array. Other interface targets are tested against
/// `System.Array`'s declarations, so an unrelated interface such as `IComparable` is rejected.
fn array_cast_matches(
    module: &Module, asm: u8, op_elem: Option<&CastElem>, op_rank: Option<u32>, token: Token,
) -> bool {
    if module.is_object_type_token(asm, token) {
        return true;
    }
    if module.is_string_type_token(asm, token) {
        return false;
    }
    match module.cast_elem(asm_key(asm, token.0)) {
        Some(CastElem::VectorInterface(target_elem) | CastElem::Array(target_elem)) => {
            op_rank.is_none()
                && op_elem.is_none_or(|op| elem_compatible(module, op, &target_elem, ElemRule::Array))
        }
        Some(CastElem::MdArray { element, rank }) => {
            op_rank == Some(rank)
                && op_elem.is_none_or(|op| elem_compatible(module, op, &element, ElemRule::Array))
        }
        Some(CastElem::Object) => true,
        Some(CastElem::Prim(_) | CastElem::String | CastElem::GenericValueType) => false,
        // A named target holds an array if it is a built-in array base or an interface declared by
        // System.Array. Optional reflection metadata can add declarations, not remove those bases.
        target @ (Some(CastElem::Named(_) | CastElem::Lenient | CastElem::Pointer(_) | CastElem::Void) | None) => {
            let handle = match target {
                Some(CastElem::Named(handle)) => handle,
                _ => asm_key(asm, token.0),
            };
            let target_id = module.type_id_of(asm, token);
            let defining_handle = target_id.and_then(|id| module.type_handle_of(id));
            // Generic operands can carry an unmarked TypeRef or omit the cast record. Their
            // resolved type keeps the array-base identity on its defining handle.
            if module.cast_names_array_base(handle)
                || defining_handle.is_some_and(|handle| module.cast_names_array_base(handle))
            {
                return true;
            }
            let reflect = defining_handle.and_then(|handle| module.reflect_type(handle));
            match (reflect, array_type_id(module), target_id) {
                (Some(reflect), Some(array_id), Some(target)) if reflect.is_interface => {
                    module.implements_interface(array_id, target)
                }
                (Some(reflect), _, _) if !reflect.is_interface => false,
                // Resolved types can omit their redundant Named cast record. Their defining
                // handles still obey the named-type rule; only unresolved shapes stay unverified.
                _ => match target {
                    Some(CastElem::Named(handle)) => module.cast_names_array_base(handle),
                    None if target_id.is_some() => module.cast_names_array_base(asm_key(asm, token.0)),
                    _ => true,
                },
            }
        }
    }
}

/// The [`TypeId`] of `System.Array`, the base of every array, or `None` when the loaded module
/// records no such type.
///
/// The reflection name must identify the loader-marked core type. An application's same-named
/// declaration cannot supply the interfaces of arrays.
fn array_type_id(module: &Module) -> Option<TypeId> {
    module
        .type_handle_by_name("System.Array")
        .filter(|handle| module.cast_names_array_base(*handle))
        .and_then(|handle| module.type_id_by_handle(handle))
}

/// Element compatibility for array casts, element addresses, and unboxing.
#[derive(Clone, Copy, PartialEq)]
enum ElemRule {
    /// Array-element compatibility (I.8.7.1): same-width signed/unsigned integers
    /// interchange, and reference elements are covariant.
    Array,
    /// `ldelema` storage compatibility: integer storage types interchange, but reference
    /// elements must be identical because the returned address permits writes.
    Address,
    /// A reference element's exact type: an array reference retains the identities of its
    /// nested elements, including signedness and enum types.
    Identity,
    /// `unbox.any` compatibility (III.4.33): the exact type, with an enum standing for its
    /// exact underlying primitive -- an `int` box does NOT unbox as `uint`.
    Unbox,
}

/// The [`CastElem`] shape of an already-asm-folded type handle: the loader's recorded
/// classification when present, else derived from the runtime type maps. An unresolvable
/// handle is [`CastElem::Lenient`] (unverified).
fn classify_elem_handle(module: &Module, handle: u64) -> CastElem {
    if let Some(elem) = module.cast_elem(handle) {
        if matches!(elem, CastElem::VectorInterface(_)) {
            return CastElem::Named(handle);
        }
        return elem;
    }
    let asm = (handle >> 32) as u8;
    let token = Token(handle as u32);
    if module.is_string_type_token(asm, token) {
        return CastElem::String;
    }
    if module.is_object_type_token(asm, token) {
        return CastElem::Object;
    }
    if module.is_enum_by_handle(handle) || module.type_id_by_handle(handle).is_some() {
        return CastElem::Named(handle);
    }
    CastElem::Lenient
}

/// The exact [`CastPrim`] a primitive value type's full name denotes, for a handle that
/// reached the matcher as [`CastElem::Named`] (a path the loader did not classify).
pub(crate) fn prim_of_full_name(full_name: &str) -> Option<CastPrim> {
    Some(match full_name {
        "System.Boolean" => CastPrim::Bool,
        "System.Char" => CastPrim::Char,
        "System.SByte" => CastPrim::I1,
        "System.Byte" => CastPrim::U1,
        "System.Int16" => CastPrim::I2,
        "System.UInt16" => CastPrim::U2,
        "System.Int32" => CastPrim::I4,
        "System.UInt32" => CastPrim::U4,
        "System.Int64" => CastPrim::I8,
        "System.UInt64" => CastPrim::U8,
        "System.Single" => CastPrim::F4,
        "System.Double" => CastPrim::F8,
        "System.IntPtr" => CastPrim::I,
        "System.UIntPtr" => CastPrim::U,
        _ => return None,
    })
}

/// Whether the type `handle` names is one that no instantiation of a generic value type can be: a
/// reference type, or a value type whose recorded full name holds no backtick, which a generic type's
/// name marks its arity with, and no bracket, which an instantiation's name holds its type arguments
/// in. A type the module cannot resolve, or whose name it did not record, is not known to be one.
fn cannot_be_a_generic_value_type(module: &Module, handle: u64) -> bool {
    let Some(type_id) = module.type_id_by_handle(handle) else {
        return false;
    };
    !module.type_is_value_type(type_id)
        || module.type_full_name(type_id).is_some_and(|name| !name.contains('`') && !name.contains('['))
}

/// Whether an operand element shape is compatible with a target element shape under `rule`.
/// Enums normalize to their underlying primitive for array casts, storage and unboxing.
/// Exact reference-type identity retains enum types and signedness inside nested arrays.
/// A named type the module cannot resolve remains unverified.
fn elem_compatible(module: &Module, op: &CastElem, target: &CastElem, rule: ElemRule) -> bool {
    let normalize = |elem: &CastElem| -> CastElem {
        if matches!(elem, CastElem::Pointer(_) | CastElem::Void) {
            return CastElem::Lenient;
        }
        if let CastElem::Named(handle) = elem {
            if rule != ElemRule::Identity {
                if let Some(prim) = module.enum_underlying_prim_by_handle(*handle) {
                    return CastElem::Prim(prim);
                }
            }
        }
        elem.clone()
    };
    let op = normalize(op);
    let target = normalize(target);
    let nested_rule = if rule == ElemRule::Address { ElemRule::Identity } else { rule };
    match (&op, &target) {
        (CastElem::Prim(_), CastElem::Lenient) | (CastElem::Lenient, CastElem::Prim(_)) => false,
        (CastElem::GenericValueType, CastElem::Named(named)) | (CastElem::Named(named), CastElem::GenericValueType) => {
            // The test is for array elements: under the unbox rule a named type matches, since `unbox.any` to a
            // reference type such as `System.ValueType` or an interface reaches here with the box's shape.
            rule == ElemRule::Unbox || !cannot_be_a_generic_value_type(module, *named)
        }
        (CastElem::GenericValueType, CastElem::GenericValueType | CastElem::Lenient)
        | (CastElem::Lenient, CastElem::GenericValueType) => true,
        (CastElem::GenericValueType, _) | (_, CastElem::GenericValueType) => false,
        (CastElem::Lenient, _) | (_, CastElem::Lenient) => true,
        (CastElem::Prim(a), CastElem::Prim(b)) => {
            a == b
                || (matches!(rule, ElemRule::Array | ElemRule::Address)
                    && matches!(
                        (a.integer_width(), b.integer_width()),
                        (Some(x), Some(y)) if x == y
                    ))
        }
        (CastElem::String, CastElem::String) | (CastElem::Object, CastElem::Object) => true,
        (CastElem::String, CastElem::Object) => rule == ElemRule::Array,
        (CastElem::String, CastElem::Named(target_handle)) => {
            rule == ElemRule::Array
                && match (
                    module.intrinsic_type_id(IntrinsicType::String),
                    module.type_id_by_handle(*target_handle),
                ) {
                    (Some(string_id), Some(target_id)) => {
                        string_id == target_id
                            || module.implements_interface(string_id, target_id)
                    }
                    _ => true,
                }
        }
        // An array element is itself a reference. Its covariance uses the same base and
        // interface rules as a directly tested array, including vector element restrictions.
        (CastElem::Array(_) | CastElem::MdArray { .. }, CastElem::Object) => rule == ElemRule::Array,
        (CastElem::Array(op_elem), CastElem::Array(target_elem)) => {
            elem_compatible(module, op_elem, target_elem, nested_rule)
        }
        (CastElem::MdArray { element: op_elem, rank: op_rank },
         CastElem::MdArray { element: target_elem, rank: target_rank }) => {
            op_rank == target_rank && elem_compatible(module, op_elem, target_elem, nested_rule)
        }
        (CastElem::Array(element), CastElem::Named(handle)) => {
            rule == ElemRule::Array
                && array_cast_matches(module, (*handle >> 32) as u8, Some(element), None, Token(*handle as u32))
        }
        (CastElem::MdArray { element, rank }, CastElem::Named(handle)) => {
            rule == ElemRule::Array
                && array_cast_matches(module, (*handle >> 32) as u8, Some(element), Some(*rank), Token(*handle as u32))
        }
        (CastElem::Named(op_handle), CastElem::Object) => {
            rule == ElemRule::Array
                && module
                    .reflect_type(*op_handle)
                    .is_none_or(|reflect| !reflect.is_value_type)
        }
        (CastElem::Named(a), CastElem::Named(b)) => {
            match (module.type_id_by_handle(*a), module.type_id_by_handle(*b)) {
                (Some(a_id), Some(b_id)) => {
                    if a_id == b_id {
                        return true;
                    }
                    rule == ElemRule::Array
                        && module
                            .reflect_type(*a)
                            .is_none_or(|reflect| !reflect.is_value_type)
                        && (module.is_subtype(a_id, b_id)
                            || module.implements_interface(a_id, b_id))
                }
                _ => true,
            }
        }
        (CastElem::Named(handle), CastElem::Prim(p))
        | (CastElem::Prim(p), CastElem::Named(handle)) => {
            if rule == ElemRule::Identity && module.is_enum_by_handle(*handle) {
                return false;
            }
            match module.reflect_type(*handle) {
                Some(reflect) => match prim_of_full_name(&reflect.full_name) {
                    Some(named_prim) => {
                        named_prim == *p
                            || (matches!(rule, ElemRule::Array | ElemRule::Address)
                                && matches!(
                                    (named_prim.integer_width(), p.integer_width()),
                                    (Some(x), Some(y)) if x == y
                                ))
                    }
                    None => false,
                },
                None => true,
            }
        },
        _ => false,
    }
}

/// Whether the box at `reference` may be unboxed as the type `token` names (III.4.32 /
/// III.4.33): the exact boxed type, or the enum/underlying interchange. A non-box reference
/// never matches (`unbox` of a string is an InvalidCastException in .NET). A side the module
/// cannot resolve is not checked, preserving cross-assembly loads with no classification.
fn unbox_matches(module: &Module, asm: u8, vm: &Vm, reference: ObjectRef, token: Token) -> bool {
    let Some(box_handle) = vm.heap().boxed_type_token(reference) else {
        return false;
    };
    if box_handle == asm_key(asm, token.0) {
        return true;
    }
    let op = classify_elem_handle(module, box_handle);
    let target = classify_elem_handle(module, asm_key(asm, token.0));
    elem_compatible(module, &op, &target, ElemRule::Unbox)
}

/// Reference equality for `ceq` / `cgt.un`: two nulls are equal, two objects are
/// equal iff they are the same reference, and a null and an object differ.
fn reference_equal(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Object(x), Value::Object(y)) => x == y,
        _ => false,
    }
}

/// The non-negative length operand of `newarr`, as a `usize`. A negative length is an
/// OverflowException, as ECMA-335 III.4.20 and .NET raise it.
fn array_length(value: Value) -> Result<usize, Trap> {
    let length = match value {
        Value::Int32(n) => i64::from(n),
        Value::Int64(n) | Value::NativeInt(n) => n,
        _ => return Err(Trap::TypeMismatch(Opcode::Newarr)),
    };
    usize::try_from(length).map_err(|_| Trap::Overflow)
}

/// The index operand of an array access, kept signed so a negative index reports as
/// out of range rather than wrapping.
fn array_index(value: Value, opcode: Opcode) -> Result<i32, Trap> {
    match value {
        Value::Int32(index) => Ok(index),
        Value::Int64(index) | Value::NativeInt(index) => {
            i32::try_from(index).map_err(|_| Trap::IndexOutOfRange(index as i32))
        }
        _ => Err(Trap::TypeMismatch(opcode)),
    }
}

/// Bounds-checks a signed array index against `len`, returning the `usize` index.
fn bounded_index(index: i32, len: usize) -> Result<usize, Trap> {
    usize::try_from(index)
        .ok()
        .filter(|&index| index < len)
        .ok_or(Trap::IndexOutOfRange(index))
}

/// Garbage-collection integration: enumerating the interpreter's roots for the heap's
/// mark-compact collector (the `gc` feature).
#[cfg(feature = "gc")]
impl Session {
    /// Reclaims unreachable objects and compacts the heap, relocating every live
    /// reference. Enumerates all roots -- each frame's eval stack, locals, arguments, and
    /// continuation state (`new_object`, `caught_exceptions`, a pending `finally` chain's
    /// exception, an in-flight multicast), the entry's result, the statics, and the
    /// exception-message table. Called at an instruction boundary, where the frame state
    /// is consistent, so anything still live is reachable from these roots.
    /// Visits every heap-reference ROOT this session holds -- each frame's eval stack, locals, args,
    /// and in-flight new-object / new-value / exception / multicast state, plus the entry's result --
    /// so the collector can mark and relocate them. Shared by the single-thread `collect_garbage` and
    /// the scheduler's all-threads collection ([`collect_all_threads`]).
    #[cfg(feature = "gc")]
    fn visit_roots(&mut self, visit: &mut dyn FnMut(&mut Value)) {
        visit_frames(&mut self.frames, visit);
        if let Some(Some(value)) = self.result.as_mut() {
            visit(value);
        }
    }

    fn collect_garbage(&mut self, module: &Module, vm: &mut Vm) {
        run_collection(vm, module, &mut |visit| self.visit_roots(visit));
    }
}

/// Visits every heap reference held by `frames` -- each frame's eval stack, locals and arguments,
/// plus its in-flight new-object / new-value / exception / multicast continuation state.
///
/// Shared by [`Session::visit_roots`] and by the parked-activation walk in [`run_collection`], and
/// that sharing is the point: a frame is a frame whether the session holding it is the one running
/// or one suspended on the Rust stack beneath a nested run, and a root the two walks disagreed
/// about would be marked on one path and missed on the other.
#[cfg(feature = "gc")]
fn visit_frames(frames: &mut [Frame], visit: &mut dyn FnMut(&mut Value)) {
    for frame in frames.iter_mut() {
        for value in frame.stack.iter_mut() {
            visit(value);
        }
        for value in frame.locals.iter_mut() {
            visit(value);
        }
        for value in frame.args.iter_mut() {
            visit(value);
        }
        visit_optional_ref(&mut frame.new_object, visit);
        visit_optional_location(&mut frame.new_value, visit);
        for (_, exception) in &mut frame.caught_exceptions {
            visit_ref(exception, visit);
        }
        for pending in &mut frame.pending {
            match &mut pending.then {
                AfterFinally::Catch { exception, .. } | AfterFinally::Unwind(exception) => {
                    visit_ref(exception, visit);
                }
                AfterFinally::Goto(_) => {}
            }
        }
        if let Some(filter) = &mut frame.pending_filter {
            visit_ref(&mut filter.exception, visit);
        }
        if let Some((invocations, params)) = &mut frame.multicast {
            for (target, _) in invocations.iter_mut() {
                visit(target);
            }
            for value in params.iter_mut() {
                visit(value);
            }
        }
    }
}

/// The shared body of a mark-compact collection, parameterized by the root walk (`roots`). The
/// single-thread [`Session::collect_garbage`] passes its own session's roots; the scheduler's
/// [`collect_all_threads`] passes every live thread's. Beyond the caller's roots it marks the
/// statics, the exception-message table, and the per-object `Monitor` lock table, then rebuilds the
/// two heap-index-keyed side tables from their relocated keys and runs any finalizers.
///
/// The lock table is the subtle one: a `Monitor` lock now spans scheduler switches (the holder can
/// be preempted inside its critical section) and therefore GCs, and its keys are heap indices the
/// compactor renumbers. So each key is mirrored as a `Value::Object` root -- marked and remapped in
/// place exactly like the exception-message keys -- and the table is rebuilt from the new indices.
/// The lock objects are independently reachable (the owner's and every waiter's frame holds the
/// `lock(obj)` reference, walked by `roots`), so this never resurrects a dead lock.
///
/// `roots` is a `dyn`, and the side tables are rebuilt by insertion, both so that the collector is
/// compiled ONCE. Taken as a generic, each caller got its own copy of the whole collector; and a
/// `collect()` into a `BTreeMap` sorts its input with a sort compiled per iterator type, so every
/// rebuild in every copy carried a sort of its own.
#[cfg(feature = "gc")]
fn run_collection(vm: &mut Vm, module: &Module, roots: &mut dyn FnMut(&mut dyn FnMut(&mut Value))) {
    #[cfg(feature = "finalizers")]
    if vm.finalizing {
        return;
    }
    #[cfg(not(feature = "finalizers"))]
    let _ = module;

    let mut messages: Vec<Value> = Vec::with_capacity(vm.exception_messages.len() * 2);
    for (&exception, &message) in &vm.exception_messages {
        messages.push(Value::Object(exception));
        messages.push(Value::Object(message));
    }
    let mut inners: Vec<Value> = Vec::with_capacity(vm.exception_inners.len() * 2);
    for (&exception, &inner) in &vm.exception_inners {
        inners.push(Value::Object(exception));
        inners.push(Value::Object(inner));
    }
    #[cfg(feature = "exceptions")]
    let mut failed_keys: Vec<TypeId> = Vec::with_capacity(vm.cctors_failed.len());
    #[cfg(feature = "exceptions")]
    let mut failed_values: Vec<Value> = Vec::with_capacity(vm.cctors_failed.len());
    #[cfg(feature = "exceptions")]
    for &(type_id, exception) in &vm.cctors_failed {
        failed_keys.push(type_id);
        failed_values.push(Value::Object(exception));
    }
    #[cfg(feature = "exceptions")]
    let mut fault_keys: Vec<Value> =
        vm.exception_faults.iter().map(|(key, _)| Value::Object(*key)).collect();

    let mut lock_states: Vec<LockState> = Vec::with_capacity(vm.locks.len());
    let mut lock_keys: Vec<Value> = Vec::with_capacity(vm.locks.len());
    for (key, state) in core::mem::take(&mut vm.locks) {
        lock_keys.push(Value::Object(ObjectRef(key)));
        lock_states.push(state);
    }

    let Vm { heap, statics, parked, reflection_monitors, .. } = vm;
    let finalizable = heap.collect(|visit| {
        roots(&mut *visit);
        for frames in parked.activations.iter_mut() {
            visit_frames(frames, visit);
        }
        for value in statics.iter_mut() {
            visit(value);
        }
        for value in messages.iter_mut() {
            visit(value);
        }
        for value in inners.iter_mut() {
            visit(value);
        }
        #[cfg(feature = "exceptions")]
        for value in failed_values.iter_mut() {
            visit(value);
        }
        #[cfg(feature = "exceptions")]
        for value in fault_keys.iter_mut() {
            visit(value);
        }
        for value in lock_keys.iter_mut() {
            visit(value);
        }
        for object in reflection_monitors.values_mut() {
            visit_ref(object, visit);
        }
    });

    vm.exception_messages = object_pairs(&messages);
    vm.exception_inners = object_pairs(&inners);
    #[cfg(feature = "exceptions")]
    for (slot, key) in vm.exception_faults.iter_mut().zip(&fault_keys) {
        if let Value::Object(reference) = key {
            slot.0 = *reference;
        }
    }
    #[cfg(feature = "exceptions")]
    {
        vm.cctors_failed = failed_keys
            .into_iter()
            .zip(failed_values)
            .filter_map(|(type_id, value)| match value {
                Value::Object(reference) => Some((type_id, reference)),
                _ => None,
            })
            .collect();
    }
    let mut locks = BTreeMap::new();
    for (key, state) in lock_keys.into_iter().zip(lock_states) {
        if let Value::Object(reference) = key {
            locks.insert(reference.0, state);
        }
    }
    vm.locks = locks;

    #[cfg(not(feature = "finalizers"))]
    let _ = finalizable;
    #[cfg(feature = "finalizers")]
    if !finalizable.is_empty() {
        vm.finalizing = true;
        for object in finalizable {
            let Some(type_id) = vm.heap().type_of(object) else {
                continue;
            };
            let Some(finalize) = module.finalizer_of(type_id) else {
                continue;
            };
            if let Ok(mut session) =
                Session::new(module, finalize, alloc::vec![Value::Object(object)])
            {
                let _ = session.run(module, vm);
            }
        }
        vm.finalizing = false;
    }
}

/// Collects with the roots of EVERY green thread, not just the running one -- the scheduler's
/// multi-thread safe point (after a thread yields/finishes, when all frames are consistent). A
/// collection while several threads are live must mark from ALL of their reified `Session` stacks (a
/// paused thread's frames hold live objects too); rooting only the running thread would reclaim the
/// others' objects. Shares [`run_collection`] with the single-thread path, walking every thread's
/// [`Session::visit_roots`].
#[cfg(feature = "gc")]
fn collect_all_threads(threads: &mut [ThreadSlot], module: &Module, vm: &mut Vm) {
    run_collection(vm, module, &mut |visit| {
        for slot in threads.iter_mut() {
            slot.session.visit_roots(visit);
        }
    });
}

/// Rebuilds an exception side table from its relocated mirror of `[key, value]` pairs.
#[cfg(feature = "gc")]
fn object_pairs(mirror: &[Value]) -> BTreeMap<ObjectRef, ObjectRef> {
    let mut table = BTreeMap::new();
    for pair in mirror.chunks_exact(2) {
        if let (Value::Object(key), Value::Object(value)) = (&pair[0], &pair[1]) {
            table.insert(*key, *value);
        }
    }
    table
}

/// Relocates an optional heap-reference root through the collector's value visitor.
#[cfg(feature = "gc")]
fn visit_optional_ref(slot: &mut Option<ObjectRef>, visit: &mut dyn FnMut(&mut Value)) {
    if let Some(reference) = slot {
        visit_ref(reference, visit);
    }
}

/// Relocates a bare heap-reference root by mirroring it through a temporary `Value`; the
/// visitor marks it, and on the relocation pass rewrites the contained `ObjectRef`.
#[cfg(feature = "gc")]
fn visit_ref(reference: &mut ObjectRef, visit: &mut dyn FnMut(&mut Value)) {
    let mut wrapped = Value::Object(*reference);
    visit(&mut wrapped);
    if let Value::Object(new) = wrapped {
        *reference = new;
    }
}

/// Relocates a managed-pointer root (a [`Location`]) by mirroring it through a temporary
/// `Value::ByRef`; the visitor marks the heap object the pointer reaches and, on the
/// relocation pass, rewrites the contained `ObjectRef`. Used for `frame.new_value` -- the
/// pointer to the under-construction value type's box, which a GC during the constructor
/// body must relocate or the struct read back on return dangles to a stale (relocated)
/// object.
#[cfg(feature = "gc")]
fn visit_location(location: &mut Location, visit: &mut dyn FnMut(&mut Value)) {
    let mut wrapped = Value::ByRef(location.clone());
    visit(&mut wrapped);
    if let Value::ByRef(new) = wrapped {
        *location = new;
    }
}

#[cfg(feature = "gc")]
fn visit_optional_location(slot: &mut Option<Location>, visit: &mut dyn FnMut(&mut Value)) {
    if let Some(location) = slot {
        visit_location(location, visit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_cil::{EhClause, Instruction, Operand};

    fn monitor_type_handles() -> (Module, [Value; 3]) {
        let mut module = Module::new();
        let first = module.add_type(Vec::new());
        let second = module.add_type(Vec::new());
        module.bind_type_token(0, Token(0x0200_0001), first);
        module.bind_type_token(1, Token(0x0100_0001), first);
        module.bind_type_token(1, Token(0x0200_0001), second);
        (module, [
            Value::NativeInt(asm_key(0, 0x0200_0001) as i64),
            Value::NativeInt(asm_key(1, 0x0100_0001) as i64),
            Value::NativeInt(asm_key(1, 0x0200_0001) as i64),
        ])
    }

    #[test]
    fn monitor_type_handles_share_aliases_but_not_other_objects() {
        use crate::intrinsics::{monitor_enter, monitor_exit, monitor_try_enter};
        for frozen in [false, true] {
            let (mut module, [first, alias, second]) = monitor_type_handles();
            if frozen { module.freeze(); }
            let mut vm = Vm::new();
            let boxed = Value::Object(vm.heap_mut().alloc_boxed(0, first.clone()));
            assert_eq!(monitor_enter(&mut vm, &module, &[first.clone()]), Ok(None));
            assert_eq!(monitor_try_enter(&mut vm, &module, &[alias.clone()]), Ok(Some(Value::Int32(1))));
            vm.set_current_thread(1);
            assert_eq!(monitor_try_enter(&mut vm, &module, &[alias.clone()]), Ok(Some(Value::Int32(0))));
            for other in [second, boxed] {
                assert_eq!(monitor_try_enter(&mut vm, &module, &[other.clone()]), Ok(Some(Value::Int32(1))));
                assert_eq!(monitor_exit(&mut vm, &module, &[other]), Ok(None));
            }
            assert_eq!(monitor_exit(&mut vm, &module, &[alias.clone()]), Err(Trap::SynchronizationLock));
            vm.set_current_thread(0);
            assert_eq!(monitor_exit(&mut vm, &module, &[alias.clone()]), Ok(None));
            vm.set_current_thread(1);
            assert_eq!(monitor_try_enter(&mut vm, &module, &[first.clone()]), Ok(Some(Value::Int32(0))));
            vm.set_current_thread(0);
            assert_eq!(monitor_exit(&mut vm, &module, &[first]), Ok(None));
            vm.set_current_thread(1);
            assert_eq!(monitor_try_enter(&mut vm, &module, &[alias.clone()]), Ok(Some(Value::Int32(1))));
            assert_eq!(monitor_exit(&mut vm, &module, &[alias]), Ok(None));
            for invalid in [Value::Null, Value::Int32(1)] {
                assert_eq!(monitor_enter(&mut vm, &module, &[invalid]), Err(Trap::TypeMismatch(Opcode::Call)));
            }
        }
    }

    #[test]
    fn monitor_constructed_types_share_structural_identity() {
        use crate::intrinsics::{monitor_enter, monitor_exit, monitor_pulse, monitor_try_enter};
        use crate::module::{CastElem, CastPrim};
        for frozen in [false, true] {
            let (mut module, [known, _, _]) = monitor_type_handles();
            let type_id = module.type_id_by_handle(asm_key(0, 0x0200_0001)).unwrap();
            let resolved_spec = Token(0x1b00_0003);
            module.bind_type_token(2, resolved_spec, type_id);
            for asm in [0, 1] {
                module.bind_cast_elem(asm, Token(0x1b00_0001),
                    CastElem::Array(Box::new(CastElem::Prim(CastPrim::I4))));
                module.bind_cast_elem(asm, Token(0x1b00_0002),
                    CastElem::MdArray { element: Box::new(CastElem::Prim(CastPrim::I4)), rank: 2 });
            }
            if frozen { module.freeze(); }
            let mut vm = Vm::new();
            vm.heap_mut().alloc_array(Vec::new());
            for token in [0x1b00_0001, 0x1b00_0002] {
                let first = Value::NativeInt(asm_key(0, token) as i64);
                let alias = Value::NativeInt(asm_key(1, token) as i64);
                assert_eq!(monitor_enter(&mut vm, &module, &[first.clone()]), Ok(None));
                assert_eq!(monitor_pulse(&mut vm, &module, &[alias.clone()]), Ok(None));
                #[cfg(feature = "gc")]
                run_collection(&mut vm, &module, &mut |_| {});
                vm.set_current_thread(1);
                assert_eq!(monitor_try_enter(&mut vm, &module, &[alias.clone()]),
                    Ok(Some(Value::Int32(0))));
                for other in [known.clone(), Value::NativeInt(asm_key(1, token ^ 3) as i64)] {
                    assert_eq!(monitor_try_enter(&mut vm, &module, &[other.clone()]),
                        Ok(Some(Value::Int32(1))));
                    assert_eq!(monitor_exit(&mut vm, &module, &[other]), Ok(None));
                }
                vm.set_current_thread(0);
                assert_eq!(monitor_exit(&mut vm, &module, &[alias]), Ok(None));
            }
            let resolved = Value::NativeInt(asm_key(2, resolved_spec.0) as i64);
            assert_eq!(monitor_enter(&mut vm, &module, &[known.clone()]), Ok(None));
            assert_eq!(monitor_pulse(&mut vm, &module, &[resolved.clone()]), Ok(None));
            vm.set_current_thread(1);
            assert_eq!(monitor_try_enter(&mut vm, &module, &[resolved]), Ok(Some(Value::Int32(0))));
            vm.set_current_thread(0);
            assert_eq!(monitor_exit(&mut vm, &module, &[known]), Ok(None));
        }
    }

    #[test]
    fn monitor_unknown_constructed_types_do_not_share_a_lenient_identity() {
        use crate::intrinsics::monitor_enter;
        for frozen in [false, true] {
            let mut module = Module::new();
            let shapes = [
                CastElem::Lenient,
                CastElem::GenericValueType,
                CastElem::Array(Box::new(CastElem::Lenient)),
                CastElem::Array(Box::new(CastElem::Named(asm_key(1, 0x0100_ffff)))),
            ];
            for (row, shape) in shapes.into_iter().enumerate() {
                module.bind_cast_elem(0, Token(0x1b00_0001 + row as u32), shape);
            }
            if frozen { module.freeze(); }
            let mut vm = Vm::new();
            for row in 1..=5 {
                let handle = Value::NativeInt(0x1b00_0000 + row);
                assert_eq!(monitor_enter(&mut vm, &module, &[handle]),
                    Err(Trap::TypeMismatch(Opcode::Call)));
            }
        }
    }

    #[test]
    fn monitor_type_handles_wait_pulse_and_timed_acquisition() {
        use crate::intrinsics::{monitor_enter, monitor_exit, monitor_pulse, monitor_pulse_all,
            monitor_try_enter, monitor_try_enter_timeout, monitor_wait, monitor_wait_timeout};
        for timed in [false, true] {
            let (module, [first, alias, _]) = monitor_type_handles();
            let mut vm = Vm::new();
            assert_eq!(monitor_enter(&mut vm, &module, &[first.clone()]), Ok(None));
            assert_eq!(monitor_enter(&mut vm, &module, &[alias.clone()]), Ok(None));
            vm.set_current_thread(1);
            assert_eq!(monitor_try_enter_timeout(&mut vm, &module, &[alias.clone(), Value::Int32(5)]), Ok(None));
            assert!(matches!(vm.thread_op.take(), Some(ThreadOp::BlockOnLock { deadline: Some(_), .. })));
            assert!(vm.resolve_lock_deadline(1));
            assert_eq!(monitor_exit(&mut vm, &module, &[alias.clone()]), Err(Trap::SynchronizationLock));
            vm.set_current_thread(0);
            let wait = if timed {
                monitor_wait_timeout(&mut vm, &module, &[first.clone(), Value::Int32(5)])
            } else {
                monitor_wait(&mut vm, &module, &[first.clone()])
            };
            assert_eq!(wait, Ok(None));
            assert!(matches!(vm.thread_op.take(), Some(ThreadOp::BlockOnLock { .. })));
            vm.set_current_thread(1);
            assert_eq!(monitor_try_enter(&mut vm, &module, &[alias.clone()]), Ok(Some(Value::Int32(1))));
            let pulse = if timed { monitor_pulse_all } else { monitor_pulse };
            assert_eq!(pulse(&mut vm, &module, &[alias.clone()]), Ok(None));
            assert_eq!(monitor_exit(&mut vm, &module, &[alias.clone()]), Ok(None));
            assert!(matches!(vm.thread_op.take(), Some(ThreadOp::WakeThread { id: 0 })));
            vm.set_current_thread(0);
            assert_eq!(monitor_exit(&mut vm, &module, &[first.clone()]), Ok(None));
            assert_eq!(monitor_exit(&mut vm, &module, &[alias.clone()]), Ok(None));
            assert_eq!(monitor_exit(&mut vm, &module, &[alias.clone()]), Err(Trap::SynchronizationLock));
            assert_eq!(monitor_wait(&mut vm, &module, &[alias.clone()]), Err(Trap::SynchronizationLock));
            assert_eq!(monitor_pulse(&mut vm, &module, &[alias]), Err(Trap::SynchronizationLock));
        }
    }

    #[cfg(feature = "gc")]
    #[test]
    fn monitor_type_handles_keep_identity_after_compaction_and_release() {
        use crate::intrinsics::{monitor_enter, monitor_exit, monitor_try_enter};
        let (module, [first, alias, second]) = monitor_type_handles();
        let mut vm = Vm::new();
        vm.heap_mut().alloc_array(Vec::new());
        assert_eq!(monitor_enter(&mut vm, &module, &[first.clone()]), Ok(None));
        run_collection(&mut vm, &module, &mut |_| {});
        vm.set_current_thread(1);
        assert_eq!(monitor_try_enter(&mut vm, &module, &[alias.clone()]), Ok(Some(Value::Int32(0))));
        assert_eq!(monitor_enter(&mut vm, &module, &[second.clone()]), Ok(None));
        vm.set_current_thread(0);
        assert_eq!(monitor_exit(&mut vm, &module, &[alias.clone()]), Ok(None));
        run_collection(&mut vm, &module, &mut |_| {});
        assert_eq!(monitor_enter(&mut vm, &module, &[first.clone()]), Ok(None));
        assert_eq!(monitor_try_enter(&mut vm, &module, &[second.clone()]), Ok(Some(Value::Int32(0))));
        vm.set_current_thread(1);
        assert_eq!(monitor_try_enter(&mut vm, &module, &[alias.clone()]), Ok(Some(Value::Int32(0))));
        assert_eq!(monitor_exit(&mut vm, &module, &[second]), Ok(None));
        vm.set_current_thread(0);
        assert_eq!(monitor_exit(&mut vm, &module, &[first]), Ok(None));
    }

    fn method(code: Vec<Instruction>) -> MethodBodyImage {
        MethodBodyImage {
            max_stack: 8,
            init_locals: true,
            local_var_sig: None,
            code: code.into_boxed_slice(),
            handlers: <Box<[EhClause]>>::default(),
        }
    }

    fn run(code: Vec<Instruction>) -> Result<Option<Value>, Trap> {
        run_method(&method(code), Vec::new())
    }

    #[cfg(feature = "exceptions")]
    fn nested_finally_trace(throw_in_try: bool) -> Option<Value> {
        let mut body = method(vec![
            Instruction::simple(Opcode::LdcI40),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(if throw_in_try { Opcode::Ldnull } else { Opcode::Nop }),
            Instruction::simple(if throw_in_try { Opcode::Throw } else { Opcode::Nop }),
            Instruction::new(Opcode::Leave, Operand::Target(21)),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::new(Opcode::LdcI4, Operand::Int32(10)),
            Instruction::simple(Opcode::Mul),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(Opcode::Endfinally),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::new(Opcode::LdcI4, Operand::Int32(10)),
            Instruction::simple(Opcode::Mul),
            Instruction::simple(Opcode::LdcI42),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(Opcode::Endfinally),
            Instruction::simple(Opcode::Pop),
            Instruction::new(Opcode::Leave, Operand::Target(21)),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::Ret),
        ]);
        body.handlers = vec![
            EhClause {
                try_range: InstructionRange { start: 2, end: 5 },
                handler_range: InstructionRange { start: 5, end: 12 },
                kind: EhKind::Finally,
            },
            EhClause {
                try_range: InstructionRange { start: 2, end: 12 },
                handler_range: InstructionRange { start: 12, end: 19 },
                kind: EhKind::Finally,
            },
            EhClause {
                try_range: InstructionRange { start: 2, end: 19 },
                handler_range: InstructionRange { start: 19, end: 21 },
                kind: EhKind::Catch(Token(0x0100_00a1)),
            },
        ]
        .into_boxed_slice();
        let mut module = Module::new();
        let main = module.add_method_image(0, body, 0);
        let mut vm = Vm::new();
        let mut session = Session::new(&module, main, Vec::new()).unwrap();
        session.continue_(&module, &mut vm).unwrap().returned
    }

    #[cfg(feature = "exceptions")]
    #[test]
    fn leave_runs_nested_finallys_with_the_same_try_start_innermost_first() {
        assert_eq!(nested_finally_trace(false), Some(Value::Int32(12)));
    }

    #[cfg(feature = "exceptions")]
    #[test]
    fn catch_runs_nested_finallys_with_the_same_try_start_innermost_first() {
        assert_eq!(nested_finally_trace(true), Some(Value::Int32(12)));
    }

    /// Discarding delivered output keeps what was not delivered, in order, keeps the buffer's
    /// storage, and opens the cap's window again -- and a VM whose embedder never discards keeps
    /// everything, as before.
    #[test]
    fn discarded_output_is_forgotten_and_the_rest_is_kept() {
        let units = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
        let mut vm = Vm::default();
        vm.write(&units("sent|kept"));
        vm.debug_write(&units("trace"));
        let capacity = vm.output.capacity();

        vm.discard_output(5);
        assert_eq!(vm.output(), units("kept"), "the undelivered tail stays, in order");
        assert_eq!(vm.output.capacity(), capacity, "the storage is reused, not given back");
        assert_eq!(vm.debug_output(), units("trace"), "each channel is discarded on its own");

        vm.discard_output(99);
        vm.discard_debug_output(99);
        assert!(vm.output().is_empty() && vm.debug_output().is_empty(), "more than there is is all");

        vm.set_output_cap(Some(4));
        vm.write(&units("abcdef"));
        assert!(vm.take_output_clipped());
        vm.discard_output(4);
        vm.write(&units("gh"));
        assert_eq!(vm.output(), units("gh"), "the window opened again once the first four went");

        let mut kept = Vm::default();
        kept.write(&units("one"));
        kept.write(&units("two"));
        assert_eq!(kept.output(), units("onetwo"), "nothing is let go unless the embedder asks");
    }

    #[test]
    fn wall_clock_advances_from_its_set_anchor() {
        use core::sync::atomic::{AtomicU64, Ordering};
        static MONO_MS: AtomicU64 = AtomicU64::new(1000);
        fn mono_ms() -> u64 {
            MONO_MS.load(Ordering::Relaxed)
        }
        fn no_sleep(_ms: u64) {}

        let mut vm = Vm::default();
        assert_eq!(vm.now_ticks(), 0);

        vm.set_clock(mono_ms, no_sleep);
        let base = 637_000_000_000_000_000i64;
        vm.set_now_ticks(base);
        assert_eq!(vm.now_ticks(), base);

        MONO_MS.store(6000, Ordering::Relaxed);
        assert_eq!(vm.now_ticks(), base + 5000 * 10_000);
    }

    /// The unset clock READS FORWARD, and a device is the whole reason: no serve firmware in this
    /// tree sets a wall clock, so this is the arm every board actually takes. A
    /// constant, which made `DateTime.UtcNow` unable to measure a duration on ANY board -- while
    /// `Environment.TickCount`, one accessor away on the SAME seam, advanced normally.
    ///
    /// Both halves are asserted because only the pair is the fix: an advancing clock whose
    /// `clock_is_set` also flipped would tell the adaptive TLS policy that a year-1 clock is
    /// trustworthy, and every certificate would fail as "not yet valid".
    #[test]
    fn an_unset_wall_clock_still_advances_and_still_reports_itself_unset() {
        use core::sync::atomic::{AtomicU64, Ordering};
        static MONO_MS: AtomicU64 = AtomicU64::new(0);
        fn mono_ms() -> u64 {
            MONO_MS.load(Ordering::Relaxed)
        }
        fn no_sleep(_ms: u64) {}

        let mut vm = Vm::default();
        assert_eq!(vm.now_ticks(), 0);
        assert!(!vm.clock_is_set());

        vm.set_clock(mono_ms, no_sleep);
        assert_eq!(vm.now_ticks(), 0);

        let start = vm.now_ticks();
        MONO_MS.store(250, Ordering::Relaxed);
        let end = vm.now_ticks();
        assert_eq!(end - start, 250 * 10_000, "an unset clock must still measure a duration");

        assert!(!vm.clock_is_set(), "advancing is not a claim about the date");

        let base = 637_000_000_000_000_000i64;
        vm.set_now_ticks(base);
        assert!(vm.clock_is_set());
        MONO_MS.store(1250, Ordering::Relaxed);
        assert_eq!(vm.now_ticks(), base + 1000 * 10_000);
    }

    #[test]
    fn wall_clock_sink_observes_every_set_and_a_late_registration() {
        use core::sync::atomic::{AtomicI64, Ordering};
        static SEEN: AtomicI64 = AtomicI64::new(0);
        fn sink(state: Option<i64>) {
            SEEN.store(state.expect("only sets happen here"), Ordering::Relaxed);
        }

        let mut vm = Vm::default();
        vm.set_wall_clock_sink(sink);
        assert_eq!(SEEN.load(Ordering::Relaxed), 0);

        vm.set_now_ticks(637_000_000_000_000_000);
        assert_eq!(SEEN.load(Ordering::Relaxed), 637_000_000_000_000_000);
        vm.set_now_ticks(638_000_000_000_000_000);
        assert_eq!(SEEN.load(Ordering::Relaxed), 638_000_000_000_000_000);

        SEEN.store(0, Ordering::Relaxed);
        let mut seeded = Vm::default();
        seeded.set_now_ticks(639_000_000_000_000_000);
        seeded.set_wall_clock_sink(sink);
        assert_eq!(SEEN.load(Ordering::Relaxed), 639_000_000_000_000_000);
    }

    #[test]
    fn native_keys_are_handle_only_and_zeroized_on_release() {
        let mut vm = Vm::default();
        let first = vm.insert_native_key(alloc::vec![0xAA; 32]);
        let second = vm.insert_native_key(alloc::vec![0xBB; 16]);
        assert_ne!(first, second);
        assert_eq!(vm.native_key(first), Some(&[0xAA; 32][..]));
        assert_eq!(vm.native_key(second), Some(&[0xBB; 16][..]));
        vm.drop_native_key(first);
        assert_eq!(vm.native_key(first), None);
        assert_eq!(vm.native_key(second), Some(&[0xBB; 16][..]));
        vm.drop_native_key(first);
        vm.drop_native_key(-1);
        assert_eq!(vm.native_key(-1), None);
    }

    #[test]
    fn subword_mmio_sim_is_aligned_word_coherent() {
        let mut vm = Vm::new();

        vm.mmio_write8(0x4100_44B5, 0x33);
        assert_eq!(vm.mmio_read8(0x4100_44B5), 0x33);

        vm.mmio_write8(0x2000_0000, 0x11);
        vm.mmio_write8(0x2000_0001, 0x22);
        vm.mmio_write8(0x2000_0002, 0x33);
        vm.mmio_write8(0x2000_0003, 0x44);
        assert_eq!(vm.mmio_read(0x2000_0000), 0x4433_2211);
        assert_eq!(vm.mmio_read8(0x2000_0002), 0x33);
        assert_eq!(vm.mmio_read8(0x2000_0011), 0x00);

        vm.mmio_write16(0x4000_0C02, 0x4018);
        assert_eq!(vm.mmio_read16(0x4000_0C02), 0x4018);
        assert_eq!(vm.mmio_read16(0x4000_0C00), 0x0000);
        assert_eq!(vm.mmio_read(0x4000_0C00), 0x4018_0000);

        vm.mmio_write16(0x2000_0100, 0xBEEF);
        assert_eq!(vm.mmio_read8(0x2000_0100), 0xEF);
        assert_eq!(vm.mmio_read8(0x2000_0101), 0xBE);

        vm.mmio_write(0x4003_1084, 0x4031_0084);
        assert_eq!(vm.mmio_read(0x4003_1084), 0x4031_0084);
    }

    #[test]
    fn evaluates_two_plus_two() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI42),
            Instruction::simple(Opcode::LdcI42),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(4))));
    }

    #[test]
    fn subtracts_and_multiplies() {
        let result = run(vec![
            Instruction::new(Opcode::LdcI4S, Operand::Int8(10)),
            Instruction::simple(Opcode::LdcI43),
            Instruction::simple(Opcode::Sub),
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::Mul),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(28))));
    }

    #[test]
    fn break_opcode_is_a_nop() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI45),
            Instruction::simple(Opcode::Break),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(5))));
    }

    #[cfg(feature = "float")]
    #[test]
    fn conv_ovf_from_nan_and_infinity_overflows() {
        for value in [
            Value::Float(f64::NAN),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NEG_INFINITY),
            Value::Single(f32::NAN),
            Value::Single(f32::INFINITY),
        ] {
            assert_eq!(convert_checked(Opcode::ConvOvfI4, value.clone()), Err(Trap::Overflow));
            assert_eq!(convert_checked(Opcode::ConvOvfU8, value), Err(Trap::Overflow));
        }
        assert_eq!(
            convert_checked(Opcode::ConvOvfI4, Value::Float(42.0)),
            Ok(Value::Int32(42))
        );
    }

    #[test]
    fn integer_divide_by_zero_traps() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::LdcI40),
            Instruction::simple(Opcode::Div),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Err(Trap::DivideByZero));
    }

    #[test]
    fn allocation_budget_guard_traps_out_of_memory_at_the_ceiling() {
        let mut vm = Vm::new();
        assert_eq!(check_alloc_budget(&vm), Ok(()));
        vm.set_object_budget(Some(2));
        assert_eq!(check_alloc_budget(&vm), Ok(()));
        vm.heap_mut().alloc_text("a");
        assert_eq!(check_alloc_budget(&vm), Ok(()));
        vm.heap_mut().alloc_text("b");
        assert_eq!(check_alloc_budget(&vm), Err(Trap::OutOfMemory));
    }

    #[test]
    #[cfg(feature = "exceptions")]
    fn out_of_memory_fault_names_the_out_of_memory_exception_hierarchy() {
        let (message, chain) =
            fault_exception(&Trap::OutOfMemory).expect("OutOfMemory is a catchable fault");
        assert_eq!(
            message,
            "Insufficient memory to continue the execution of the program."
        );
        assert_eq!(chain.first(), Some(&"System.OutOfMemoryException"));
        assert_eq!(chain.last(), Some(&"System.Object"));
        assert!(chain.contains(&"System.SystemException"));
        assert!(chain.contains(&"System.Exception"));
    }

    /// The unencodable-unit fault names `EncoderFallbackException` AND `ArgumentException`, which
    /// is why that pairing matters: a `catch (ArgumentException)` written against desktop .NET
    /// fires on it without naming a type that framework's author never wrote.
    #[test]
    #[cfg(feature = "exceptions")]
    fn the_encoder_fallback_fault_is_also_an_argument_exception() {
        let (_, chain) = fault_exception(&Trap::EncoderFallback {
            char_unknown: 0xD800,
            index: 1,
        })
        .expect("an unencodable unit is a catchable fault");
        assert_eq!(chain.first(), Some(&"System.Text.EncoderFallbackException"));
        assert!(chain.contains(&"System.ArgumentException"));
        assert_eq!(chain.last(), Some(&"System.Object"));
    }

    /// The message is .NET's OWN text for this fault, character for character -- read off a real
    /// `EncoderFallbackException` from `Encoding.GetBytes` under `EncoderExceptionFallback`, not
    /// reconstructed from the shape of the sentence.
    ///
    /// The DOUBLED BACKSLASH is deliberate and is the reason this test spells the string out
    /// rather than describing it: .NET's resource really does emit `\\uD800`, and a reader who
    /// "fixes" it to one backslash silently stops matching the desktop message.
    ///
    /// It is also the only place the diagnostic is asserted at all. Managed code cannot read it
    /// back yet: `Exception.Message` on ANY runtime-raised exception traps, because such an
    /// exception carries no field storage while corlib's getter reads a field. That is a separate,
    /// older defect -- a plain divide-by-zero reproduces it -- so the text is pinned here.
    #[test]
    #[cfg(feature = "exceptions")]
    fn the_encoder_fallback_message_is_dotnets_own_text() {
        let (message, _) = fault_exception(&Trap::EncoderFallback {
            char_unknown: 0xD800,
            index: 1,
        })
        .expect("an unencodable unit is a catchable fault");
        assert_eq!(
            message,
            "Unable to translate Unicode character \\\\uD800 at index 1 to specified code page."
        );
    }

    #[cfg(feature = "exceptions")]
    #[test]
    fn allocation_over_budget_raises_a_catchable_out_of_memory_exception() {
        use lamella_cil::{EhClause, EhKind, InstructionRange};
        let elem = Token(0x0100_0001);
        let mut module = Module::new();
        module.bind_array_default(0, elem, Value::Int32(0));
        let mut code = Vec::new();
        for _ in 0..4 {
            code.push(Instruction::simple(Opcode::LdcI41));
            code.push(Instruction::new(Opcode::Newarr, Operand::Token(elem)));
        }
        let handler_start = code.len() as u32;
        code.push(Instruction::simple(Opcode::Pop));
        code.push(Instruction::new(Opcode::LdcI4S, Operand::Int8(42)));
        code.push(Instruction::simple(Opcode::Ret));
        let handler_end = code.len() as u32;
        let mut body = method(code);
        body.handlers = alloc::vec![EhClause {
            try_range: InstructionRange {
                start: 0,
                end: handler_start,
            },
            handler_range: InstructionRange {
                start: handler_start,
                end: handler_end,
            },
            kind: EhKind::Catch(Token(0x0100_00FF)),
        }]
        .into_boxed_slice();
        let main = module.add_method_image(0, body, 0);

        let mut vm = Vm::new();
        vm.set_object_budget(Some(3));
        assert_eq!(
            super::run(&module, &mut vm, main, Vec::new()),
            Ok(Some(Value::Int32(42)))
        );
    }

    #[test]
    fn add_wraps_around_like_twos_complement() {
        let result = run(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(i32::MAX)),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(i32::MIN))));
    }

    #[test]
    fn localloc_zero_inits_and_round_trips_an_int() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::Dup),
            Instruction::new(Opcode::LdcI4, Operand::Int32(0x1234_5678)),
            Instruction::simple(Opcode::StindI4),
            Instruction::simple(Opcode::LdindI4),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(0x1234_5678))));
    }

    fn stack_struct_field_program(code: Vec<Instruction>) -> (Module, MethodId) {
        let mut module = Module::new();
        let record = module.add_type(vec![Value::Int32(0), Value::Int32(0)]);
        module.set_type_is_value_type(record, true);
        module.bind_type_token(0, Token(0x0200_0001), record);
        module.set_type_field_offsets(record, 8, 4, &[0, 4]);
        module.set_type_field_storage(record,
            &[crate::module::FieldStorage::Primitive(crate::PrimKind::I4); 2]);
        let field = Token(0x0400_0001);
        module.bind_field(0, field, 1);
        module.bind_field_type(0, field, record);
        let entry = module.add_method_image(0, method(code), 0);
        (module, entry)
    }

    #[test]
    fn stack_struct_field_store_uses_byte_offset() {
        let (module, entry) = stack_struct_field_program(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(16)),
            Instruction::simple(Opcode::Localloc),
            Instruction::new(Opcode::LdcI4, Operand::Int32(8)),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Dup),
            Instruction::new(Opcode::LdcI4, Operand::Int32(47)),
            Instruction::new(Opcode::Stfld, Operand::Token(Token(0x0400_0001))),
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::LdindI4),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(super::run(&module, &mut Vm::new(), entry, Vec::new()),
            Ok(Some(Value::Int32(47))));
    }

    #[test]
    fn stack_intrinsic_arguments_trap_instead_of_becoming_null() {
        fn observe(_vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
            Ok(args.first().cloned())
        }
        for cross_frame in [false, true] {
            for frozen in [false, true] {
                let mut module = Module::new();
                let intrinsic_token = Token(0x0a00_0001);
                let relay_token = Token(0x0600_0001);
                let intrinsic = module.add_intrinsic(0, observe, 0, 1);
                module.bind_token(0, intrinsic_token, intrinsic);
                let relay = module.add_method_image(0, method(vec![
                    Instruction::simple(Opcode::Ldarg0),
                    Instruction::new(Opcode::Call, Operand::Token(intrinsic_token)),
                    Instruction::simple(Opcode::Ret),
                ]), 1);
                module.bind_token(0, relay_token, relay);
                let target = if cross_frame { relay_token } else { intrinsic_token };
                let entry = module.add_method_image(0, method(vec![
                    Instruction::simple(Opcode::LdcI44),
                    Instruction::simple(Opcode::Localloc),
                    Instruction::simple(Opcode::Dup),
                    Instruction::new(Opcode::LdcI4, Operand::Int32(42)),
                    Instruction::simple(Opcode::StindI4),
                    Instruction::new(Opcode::Call, Operand::Token(target)),
                    Instruction::simple(Opcode::Ret),
                ]), 0);
                if frozen { module.freeze(); }
                assert_eq!(super::run(&module, &mut Vm::new(), entry, Vec::new()),
                    Err(Trap::Unsupported(Opcode::Call)), "cross_frame={cross_frame} frozen={frozen}");
            }
        }
    }

    #[test]
    fn readable_intrinsic_arguments_preserve_values_including_null() {
        fn observe(_vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
            Ok(args.first().cloned())
        }
        for (value, expected) in [
            (Instruction::new(Opcode::LdcI4, Operand::Int32(42)), Value::Int32(42)),
            (Instruction::simple(Opcode::Ldnull), Value::Null),
        ] {
            let mut module = Module::new();
            let token = Token(0x0a00_0001);
            let intrinsic = module.add_intrinsic(0, observe, 0, 1);
            module.bind_token(0, token, intrinsic);
            let entry = module.add_method_image(0, method(vec![
                value,
                Instruction::simple(Opcode::Stloc0),
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::new(Opcode::Call, Operand::Token(token)),
                Instruction::simple(Opcode::Ret),
            ]), 0);
            for frozen in [false, true] {
                if frozen { module.freeze(); }
                assert_eq!(super::run(&module, &mut Vm::new(), entry, Vec::new()),
                    Ok(Some(expected.clone())), "frozen={frozen}");
            }
        }
    }

    #[test]
    fn stack_struct_whole_object_loads_do_not_return_null() {
        for cross_frame in [false, true] {
            let kind = Token(0x0200_0001);
            let callee_token = Token(0x0600_0001);
            let mut code = vec![
                Instruction::simple(Opcode::LdcI48),
                Instruction::simple(Opcode::Localloc),
                Instruction::simple(Opcode::Dup),
                Instruction::new(Opcode::LdcI4, Operand::Int32(20)),
                Instruction::new(Opcode::Stfld, Operand::Token(Token(0x0400_0001))),
            ];
            code.push(if cross_frame {
                Instruction::new(Opcode::Call, Operand::Token(callee_token))
            } else {
                Instruction::new(Opcode::Ldobj, Operand::Token(kind))
            });
            code.push(Instruction::simple(Opcode::Ret));
            let (mut module, entry) = stack_struct_field_program(code);
            let callee = module.add_method_image(0, method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::new(Opcode::Ldobj, Operand::Token(kind)),
                Instruction::simple(Opcode::Ret),
            ]), 1);
            module.bind_token(0, callee_token, callee);
            assert_eq!(super::run(&module, &mut Vm::new(), entry, Vec::new()),
                Err(Trap::Unsupported(Opcode::Ldobj)), "cross_frame={cross_frame}");
        }
    }

    #[test]
    fn stack_struct_whole_object_copies_do_not_write_null() {
        let (module, entry) = stack_struct_field_program(vec![
            Instruction::simple(Opcode::LdcI48),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::Dup),
            Instruction::new(Opcode::LdcI4, Operand::Int32(20)),
            Instruction::new(Opcode::Stfld, Operand::Token(Token(0x0400_0001))),
            Instruction::simple(Opcode::Stloc0),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(1)),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::new(Opcode::Cpobj, Operand::Token(Token(0x0200_0001))),
            Instruction::simple(Opcode::Ldloc1),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(super::run(&module, &mut Vm::new(), entry, Vec::new()),
            Err(Trap::Unsupported(Opcode::Cpobj)));
    }

    #[test]
    fn stack_struct_field_load_reads_raw_bytes() {
        let (module, entry) = stack_struct_field_program(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(8)),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::Dup),
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::Add),
            Instruction::new(Opcode::LdcI4, Operand::Int32(47)),
            Instruction::simple(Opcode::StindI4),
            Instruction::new(Opcode::Ldfld, Operand::Token(Token(0x0400_0001))),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(super::run(&module, &mut Vm::new(), entry, Vec::new()),
            Ok(Some(Value::Int32(47))));
    }

    #[test]
    fn stack_struct_field_address_reaches_raw_bytes() {
        let (module, entry) = stack_struct_field_program(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(8)),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::Dup),
            Instruction::new(Opcode::Ldflda, Operand::Token(Token(0x0400_0001))),
            Instruction::new(Opcode::LdcI4, Operand::Int32(47)),
            Instruction::simple(Opcode::StindI4),
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::LdindI4),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(super::run(&module, &mut Vm::new(), entry, Vec::new()),
            Ok(Some(Value::Int32(47))));
    }

    fn stack_struct_cross_frame_program(mut module: Module, storage: crate::module::FieldStorage) -> (Module, MethodId) {
        let record = module.add_type(vec![Value::Null]);
        module.set_type_is_value_type(record, true);
        module.set_type_field_offsets(record, 24, 8, &[8]);
        module.set_type_field_storage(record, &[storage]);
        let field = Token(0x0400_0001);
        module.bind_field(1, field, 0);
        module.bind_field_type(1, field, record);
        let callee_token = Token(0x0600_0001);
        let callee = module.add_method_image(1, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::Ldarg1),
            Instruction::new(Opcode::Stfld, Operand::Token(field)),
            Instruction::simple(Opcode::Ldarg0),
            Instruction::new(Opcode::Ldfld, Operand::Token(field)),
            Instruction::simple(Opcode::Ret),
        ]), 2);
        module.bind_token(0, callee_token, callee);
        let main = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(24)),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::Ldarg0),
            Instruction::new(Opcode::Call, Operand::Token(callee_token)),
            Instruction::simple(Opcode::Ret),
        ]), 1);
        (module, main)
    }

    fn check_stack_struct_storage_modes(mut module: Module, entry: MethodId, value: Value, expected: Value) {
        let check = |module: &Module| assert_eq!(
            super::run(module, &mut Vm::new(), entry, vec![value.clone()]), Ok(Some(expected.clone())));
        check(&module);
        #[cfg(feature = "code-in-place")]
        {
            let mut trimmed = module.clone();
            let (methods, types, strings) = trimmed.reachable_set(Some(entry));
            trimmed.retain_reachable(&methods, &types, &strings);
            check(&trimmed);
            let image = trimmed.write_baked(Some(entry)).expect("bake trimmed raw fields");
            let (baked, _) = Module::from_baked(Box::leak(image.into_boxed_slice())).unwrap();
            check(&baked);
        }
        module.freeze();
        check(&module);
        #[cfg(feature = "code-in-place")]
        {
            let image = module.write_baked(Some(entry)).expect("bake raw fields");
            let (baked, _) = Module::from_baked(Box::leak(image.into_boxed_slice())).unwrap();
            check(&baked);
        }
    }

    #[test]
    fn stack_struct_fields_use_declared_integer_widths_across_frames() {
        use crate::{module::FieldStorage, PrimKind};
        for (kind, value, expected) in [
            (PrimKind::I1, Value::Int32(0x1ff), Value::Int32(-1)),
            (PrimKind::U1, Value::Int32(0x1ff), Value::Int32(255)),
            (PrimKind::I2, Value::Int32(0x18001), Value::Int32(-32767)),
            (PrimKind::U2, Value::Int32(0x18001), Value::Int32(32769)),
            (PrimKind::I4, Value::Int32(i32::MIN), Value::Int32(i32::MIN)),
            (PrimKind::I8, Value::Int64(i64::MIN), Value::Int64(i64::MIN)),
        ] {
            let (module, entry) = stack_struct_cross_frame_program(Module::new(), FieldStorage::Primitive(kind));
            check_stack_struct_storage_modes(module, entry, value, expected);
        }
        let (module, entry) = stack_struct_cross_frame_program(Module::new(), FieldStorage::Native);
        check_stack_struct_storage_modes(module.clone(), entry, Value::NativeInt(0x1234), Value::NativeInt(0x1234));
        if crate::native_pointer_size() == 4 {
            assert_eq!(super::run(&module, &mut Vm::new(), entry, vec![Value::NativeInt(-1)]),
                Err(Trap::Unsupported(Opcode::Ldfld)));
        } else {
            check_stack_struct_storage_modes(module, entry, Value::NativeInt(-1), Value::NativeInt(-1));
        }
    }

    #[cfg(feature = "float")]
    #[test]
    fn stack_struct_fields_preserve_floating_storage_across_frames() {
        use crate::{module::FieldStorage, PrimKind};
        for (kind, value, expected) in [
            (PrimKind::F4, Value::Float(1.0 / 3.0), Value::Single(1.0 / 3.0)),
            (PrimKind::F8, Value::Float(-1.25), Value::Float(-1.25)),
        ] {
            let (module, entry) = stack_struct_cross_frame_program(Module::new(), FieldStorage::Primitive(kind));
            check_stack_struct_storage_modes(module, entry, value, expected);
        }
    }

    #[test]
    fn stack_struct_nested_field_copies_keep_layout_dependencies() {
        use crate::{module::FieldStorage, PrimKind};
        let mut module = Module::new();
        let nested = module.add_type(vec![Value::Int32(0), Value::Int32(0)]);
        module.set_type_field_offsets(nested, 8, 4, &[0, 4]);
        module.set_type_field_storage(nested, &[
            FieldStorage::Primitive(PrimKind::I1), FieldStorage::Primitive(PrimKind::I4)]);
        let (module, entry) = stack_struct_cross_frame_program(module, FieldStorage::ValueType(nested));
        let value = Value::Struct(vec![Value::Int32(255), Value::Int32(47)].into_boxed_slice());
        let expected = Value::Struct(vec![Value::Int32(-1), Value::Int32(47)].into_boxed_slice());
        check_stack_struct_storage_modes(module, entry, value, expected);
    }

    #[test]
    fn stack_struct_narrow_writes_preserve_neighbors_and_padding() {
        use crate::{module::FieldStorage, PrimKind};
        let mut module = Module::new();
        let record = module.add_type(vec![Value::Int32(0)]);
        module.set_type_field_offsets(record, 4, 4, &[1]);
        module.set_type_field_storage(record, &[FieldStorage::Primitive(PrimKind::U1)]);
        let field = Token(0x0400_0001);
        module.bind_field(0, field, 0);
        module.bind_field_type(0, field, record);
        let entry = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::Dup),
            Instruction::new(Opcode::LdcI4, Operand::Int32(0x12345678)),
            Instruction::simple(Opcode::StindI4),
            Instruction::simple(Opcode::Dup),
            Instruction::new(Opcode::LdcI4, Operand::Int32(0x1ab)),
            Instruction::new(Opcode::Stfld, Operand::Token(field)),
            Instruction::simple(Opcode::LdindI4),
            Instruction::simple(Opcode::Ret),
        ]), 0);
        assert_eq!(super::run(&module, &mut Vm::new(), entry, Vec::new()),
            Ok(Some(Value::Int32(0x1234ab78))));
    }

    #[test]
    fn stack_struct_invalid_layouts_and_out_of_bounds_accesses_trap() {
        use crate::{module::FieldStorage, PrimKind};
        let (mut module, entry) = stack_struct_cross_frame_program(Module::new(), FieldStorage::Primitive(PrimKind::I4));
        module.set_type_field_offsets(0, 28, 4, &[24]);
        assert_eq!(super::run(&module, &mut Vm::new(), entry, vec![Value::Int32(47)]), Err(Trap::NullReference));
        module.set_type_field_offsets(0, 8, 4, &[8]);
        assert_eq!(super::run(&module, &mut Vm::new(), entry, vec![Value::Int32(47)]), Err(Trap::Unsupported(Opcode::Stfld)));
        module.set_type_field_offsets(0, 24, 4, &[8]);
        module.set_type_field_storage(0, &[]);
        assert_eq!(super::run(&module, &mut Vm::new(), entry, vec![Value::Int32(47)]), Err(Trap::Unsupported(Opcode::Stfld)));
        module.set_type_field_storage(0, &[FieldStorage::Reference]);
        assert_eq!(super::run(&module, &mut Vm::new(), entry, vec![Value::Null]), Err(Trap::Unsupported(Opcode::Stfld)));
        module.set_type_field_storage(0, &[FieldStorage::Native]);
        let pointer = Value::ByRef(Location::Local { frame: 0, slot: 0 });
        assert_eq!(super::run(&module, &mut Vm::new(), entry, vec![pointer]), Err(Trap::TypeMismatch(Opcode::StindI)));
        module.set_type_field_offsets(0, 4, 4, &[0]);
        module.set_type_field_storage(0, &[FieldStorage::ValueType(0)]);
        assert_eq!(decode_stack_field(&module, FieldStorage::ValueType(0), &[0; 4], 0), Err(Trap::Unsupported(Opcode::Ldfld)));
    }

    #[test]
    fn localloc_unwritten_bytes_read_as_zero() {
        let result = run(vec![
            Instruction::new(Opcode::LdcI4S, Operand::Int8(8)),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::LdindI4),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(0))));
    }

    #[test]
    fn localloc_pointer_add_indexes_bytes_little_endian() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::new(Opcode::LdcI4, Operand::Int32(0xCD)),
            Instruction::simple(Opcode::StindI1),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Add),
            Instruction::new(Opcode::LdcI4, Operand::Int32(0xAB)),
            Instruction::simple(Opcode::StindI1),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::LdindU2),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(0xABCD))));
    }

    fn convert_value(vm: &mut Vm, opcode: Opcode, value: Value) -> Result<Option<Value>, Trap> {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(opcode),
            Instruction::simple(Opcode::Ret),
        ]), 1);
        super::run(&module, vm, main, vec![value])
    }

    #[test]
    fn fixed_string_native_conversion_preserves_character_addresses() {
        for opcode in [Opcode::ConvI, Opcode::ConvU] {
            for text in ["s\0t!", "", "\u{e9}\u{1f642}"] {
                let mut vm = Vm::new();
                vm.heap_mut().alloc_text("discarded before collection");
                let string = vm.heap_mut().alloc_text(text);
                let pointer = convert_value(&mut vm, opcode, Value::Object(string))
                    .expect("convert pinned string").expect("native pointer");
                assert_eq!(pointer, Value::ByRef(Location::StringChar { string, byte_offset: 0 }));
                let mut units: Vec<u16> = text.encode_utf16().collect();
                units.push(0);
                for (index, expected) in units.iter().enumerate() {
                    let displaced = stack_pointer_arithmetic(vm.heap(), Opcode::Add,
                        &pointer, &Value::Int32((index * 2) as i32)).unwrap().unwrap();
                    let Value::ByRef(location) = displaced else { panic!("string address") };
                    assert_eq!(read_location_value(&[], &vm, location), Some(Value::Int32(i32::from(*expected))));
                }
                assert_eq!(convert_value(&mut vm, opcode, pointer.clone()), Ok(Some(pointer.clone())));
                #[cfg(feature = "gc")]
                {
                    let mut root = pointer;
                    vm.heap_mut().collect(|visit| visit(&mut root));
                    let Value::ByRef(location @ Location::StringChar { string: moved, .. }) = root else {
                        panic!("string address lost during collection");
                    };
                    assert_ne!(moved, string, "the retained string must move");
                    assert_eq!(read_location_value(&[], &vm, location), Some(Value::Int32(i32::from(units[0]))));
                }
            }
        }
    }

    #[test]
    fn fixed_string_null_conversion_and_invalid_reference_controls() {
        let mut vm = Vm::new();
        let string = Value::Object(vm.heap_mut().alloc_text("test"));
        let object = Value::Object(vm.heap_mut().alloc_instance(0, vec![]));
        for opcode in [Opcode::ConvI, Opcode::ConvU] {
            assert_eq!(convert_value(&mut vm, opcode, Value::Null), Ok(Some(Value::NativeInt(0))));
            assert_eq!(convert_value(&mut vm, opcode, object.clone()), Err(Trap::TypeMismatch(opcode)));
            assert_eq!(convert_value(&mut vm, opcode, Value::Int32(42)), Ok(Some(Value::NativeInt(42))));
        }
        for opcode in [Opcode::ConvI1, Opcode::ConvU2, Opcode::ConvI4, Opcode::ConvI8, Opcode::ConvR8] {
            for value in [Value::Null, string.clone(), object.clone()] {
                assert_eq!(convert_value(&mut vm, opcode, value), Err(Trap::TypeMismatch(opcode)));
            }
        }
    }

    #[test]
    fn fixed_string_offset_intrinsic_matches_the_symbolic_data_address() {
        use crate::intrinsic_registry::{intrinsic_by_id, intrinsic_id};
        let getter = intrinsic_by_id(intrinsic_id("runtime_helpers_offset_to_string_data"))
            .expect("registered string data offset getter");
        let mut vm = Vm::new();
        assert_eq!(getter(&mut vm, &Module::new(), &[]), Ok(Some(Value::Int32(0))));
    }

    #[test]
    fn string_pinnable_pointer_reads_units_terminator_and_walks() {
        let mut vm = Vm::new();
        let s = vm.heap_mut().alloc_text("*+");
        let at = |offset: u32| {
            read_location_value(
                &[],
                &vm,
                Location::StringChar {
                    string: s,
                    byte_offset: offset,
                },
            )
        };
        assert_eq!(at(0), Some(Value::Int32(42)));
        assert_eq!(at(2), Some(Value::Int32(43)));
        assert_eq!(at(4), Some(Value::Int32(0)));
        assert_eq!(at(6), None);
        let pinned = Value::ByRef(Location::StringChar {
            string: s,
            byte_offset: 0,
        });
        let walked =
            stack_pointer_arithmetic(vm.heap(), Opcode::Add, &pinned, &Value::Int32(2)).unwrap();
        assert_eq!(
            walked,
            Some(Value::ByRef(Location::StringChar {
                string: s,
                byte_offset: 2,
            }))
        );
        let empty = vm.heap_mut().alloc_text("");
        assert_eq!(
            read_location_value(
                &[],
                &vm,
                Location::StringChar {
                    string: empty,
                    byte_offset: 0,
                },
            ),
            Some(Value::Int32(0))
        );
    }

    fn array_pointer_binary(
        vm: &mut Vm,
        opcode: Opcode,
        a: Value,
        b: Value,
    ) -> Result<Option<Value>, Trap> {
        let mut module = Module::new();
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::simple(opcode),
                Instruction::simple(Opcode::Ret),
            ]),
            2,
        );
        super::run(&module, vm, main, vec![a, b])
    }

    #[test]
    fn array_pointer_comparisons_use_byte_addresses() {
        use crate::object::PrimKind;
        for kind in [PrimKind::U1, PrimKind::I2, PrimKind::I4, PrimKind::I8] {
            let mut vm = Vm::new();
            let array = vm.heap_mut().alloc_packed_array(kind, 4, 0);
            let other = vm.heap_mut().alloc_packed_array(kind, 4, 0);
            let width = kind.byte_width() as u32;
            let at = |array, index, byte_offset| {
                Value::ByRef(Location::Element { array, index, byte_offset })
            };
            let indexed = at(array, 2, 0);
            let displaced = at(array, 1, width);
            for (a, b, opcode, expected) in [
                (indexed.clone(), displaced.clone(), Opcode::Ceq, 1),
                (displaced.clone(), indexed.clone(), Opcode::Ceq, 1),
                (at(array, 0, width + 1), at(array, 1, 0), Opcode::CgtUn, 1),
                (at(array, 1, 0), at(array, 0, width + 1), Opcode::CltUn, 1),
                (indexed.clone(), at(other, 2, 0), Opcode::Ceq, 0),
                (indexed, at(array, 2, 1), Opcode::Ceq, 0),
            ] {
                assert_eq!(
                    array_pointer_binary(&mut vm, opcode, a, b),
                    Ok(Some(Value::Int32(expected))),
                    "{kind:?} {opcode:?}"
                );
            }
        }
    }

    #[test]
    fn array_pointer_difference_uses_byte_addresses() {
        use crate::object::PrimKind;
        for kind in [PrimKind::U1, PrimKind::I2, PrimKind::I4, PrimKind::I8] {
            let mut vm = Vm::new();
            let array = vm.heap_mut().alloc_packed_array(kind, 4, 0);
            let width = kind.byte_width() as u32;
            let at = |index, byte_offset| {
                Value::ByRef(Location::Element { array, index, byte_offset })
            };
            for (a, b, expected) in [
                (at(2, 0), at(0, 0), i64::from(2 * width)),
                (at(0, 0), at(2, 0), -i64::from(2 * width)),
                (at(1, width), at(2, 0), 0),
            ] {
                assert_eq!(
                    array_pointer_binary(&mut vm, Opcode::Sub, a, b),
                    Ok(Some(Value::NativeInt(expected))),
                    "{kind:?}"
                );
            }
        }
    }

    #[test]
    fn struct_array_pointers_use_padded_element_layout() {
        let definition = Token::new(0x02, 1);
        let reference = Token::new(0x01, 1);
        for preparation in ["live", "frozen", "baked"] {
            let mut module = Module::new();
            let item = module.add_type(vec![Value::Int32(0); 3]);
            module.set_type_is_value_type(item, true);
            module.bind_type_token(0, definition, item);
            module.bind_type_token(1, reference, item);
            module.set_type_size(0, definition, 12);
            module.set_type_field_offsets(item, 12, 4, &[0, 4, 8]);
            if preparation == "frozen" {
                module.freeze();
            }
            if preparation == "baked" {
                #[cfg(not(feature = "code-in-place"))]
                continue;
                #[cfg(feature = "code-in-place")]
                {
                    let image = module.write_baked(None).expect("bake array element layout");
                    module = Module::from_baked(Box::leak(image.into_boxed_slice()))
                        .expect("read array element layout").0;
                }
            }
            for (asm, token) in [(0, definition), (1, reference)] {
                let mut vm = Vm::new();
                let array = alloc_array_of_element(&mut vm, &module, asm, token, 4);
                assert_eq!(vm.heap().array_element_width(array), Some(12));
                let empty = alloc_array_of_element(&mut vm, &module, asm, token, 0);
                assert_eq!(vm.heap().array_element_width(empty), Some(12));
                let value = |n| Value::Struct(vec![Value::Int32(n); 3].into_boxed_slice());
                for index in 0..4 {
                    assert!(vm.heap_mut().array_set(array, index, value(index as i32 + 10)));
                }
                let at = |index, byte_offset| Location::Element { array, index, byte_offset };
                for (a, b, opcode, expected) in [
                    (at(2, 0), at(0, 0), Opcode::Sub, Value::NativeInt(24)),
                    (at(0, 0), at(2, 0), Opcode::Sub, Value::NativeInt(-24)),
                    (at(0, 24), at(2, 0), Opcode::Ceq, Value::Int32(1)),
                    (at(0, 25), at(2, 0), Opcode::CgtUn, Value::Int32(1)),
                ] {
                    assert_eq!(array_pointer_binary(&mut vm, opcode, Value::ByRef(a), Value::ByRef(b)),
                        Ok(Some(expected)), "{preparation}, asm={asm}, {opcode:?}");
                }
                assert_eq!(read_location_value(&[], &vm, at(0, 24)), Some(value(12)));
                write_location_value(&mut [], &mut vm, at(0, 36), value(99)).expect("write third stride");
                assert_eq!(vm.heap().array_get(array, 2), Some(value(12)));
                assert_eq!(vm.heap().array_get(array, 3), Some(value(99)));
                let copy = vm.heap_mut().clone_array(array).expect("clone struct array");
                assert_eq!(vm.heap().array_element_width(copy), Some(12));
                assert_eq!(read_location_value(&[], &vm, Location::Element {
                    array: copy, index: 0, byte_offset: 36,
                }), Some(value(99)));
                #[cfg(feature = "gc")]
                {
                    let mut root = Value::ByRef(Location::Element {
                        array: copy, index: 0, byte_offset: 36,
                    });
                    vm.heap_mut().collect(|visit| visit(&mut root));
                    let Value::ByRef(location @ Location::Element { array: moved, .. }) = root else {
                        panic!("element address lost during collection");
                    };
                    assert_ne!(moved, copy, "the retained array must move");
                    assert_eq!(vm.heap().array_element_width(moved), Some(12));
                    assert_eq!(read_location_value(&[], &vm, location), Some(value(99)));
                }
            }
        }
    }

    #[test]
    fn struct_array_stride_uses_layout_without_guessing_from_slots() {
        let token = Token::new(0x02, 1);
        let mut module = Module::new();
        let item = module.add_type(vec![Value::Int32(0); 3]);
        module.set_type_is_value_type(item, true);
        module.bind_type_token(0, token, item);
        let mut vm = Vm::new();
        let unknown = alloc_array_of_element(&mut vm, &module, 0, token, 2);
        assert_eq!(vm.heap().array_element_width(unknown), None);
        // The same reduced slots can describe a byte, short and int with padding.
        module.set_type_field_offsets(item, 8, 4, &[0, 2, 4]);
        module.freeze();
        let recorded = alloc_array_of_element(&mut vm, &module, 0, token, 2);
        assert_eq!(vm.heap().array_element_width(recorded), Some(8));
    }

    /// A module holding `Pair`, a class whose two `int` fields the loader recorded at bytes 0 and 4,
    /// and an instance of it holding 5 and 7.
    fn walkable_pair() -> (Module, Vm, ObjectRef) {
        let mut module = Module::new();
        let pair = module.add_type(alloc::vec![Value::Int32(0), Value::Int32(0)]);
        module.bind_type_full_name(pair, String::from("Pair"));
        module.set_type_field_offsets(pair, 8, 4, &[0, 4]);
        let mut vm = Vm::new();
        let object = vm
            .heap_mut()
            .alloc_instance(pair, alloc::vec![Value::Int32(5), Value::Int32(7)]);
        (module, vm, object)
    }

    /// The refusal a field-pointer walk raised, or a panic naming what it did instead.
    fn walk_refusal(result: Result<Option<Value>, Trap>) -> String {
        match result {
            Err(Trap::FieldPointerWalk(message)) => message.into_owned(),
            other => panic!("expected a refused walk, got {other:?}"),
        }
    }

    #[test]
    fn a_field_address_walks_to_the_field_that_starts_at_the_byte_it_reaches() {
        let (module, vm, object) = walkable_pair();
        let field = |slot| Value::ByRef(Location::Field { object, slot });
        let walk = |opcode, a: &Value, b: &Value| {
            field_pointer_arithmetic(Some(&module), &vm, opcode, a, b)
        };
        assert_eq!(walk(Opcode::Add, &field(0), &Value::NativeInt(4)), Ok(Some(field(1))));
        assert_eq!(walk(Opcode::Add, &Value::Int32(4), &field(0)), Ok(Some(field(1))));
        assert_eq!(walk(Opcode::Sub, &field(1), &Value::Int32(4)), Ok(Some(field(0))));
        assert_eq!(walk(Opcode::Add, &field(0), &Value::Int32(8)), Ok(Some(field(2))));
        assert_eq!(walk(Opcode::Sub, &field(1), &field(0)), Ok(Some(Value::NativeInt(4))));
        assert_eq!(walk(Opcode::Mul, &field(0), &Value::Int32(4)), Ok(None));
    }

    #[test]
    fn a_walk_that_lands_inside_a_field_or_outside_the_object_is_refused_by_name() {
        let (module, vm, object) = walkable_pair();
        let field = Value::ByRef(Location::Field { object, slot: 0 });
        for offset in [1, 6, 12, -4] {
            let message = walk_refusal(field_pointer_arithmetic(
                Some(&module),
                &vm,
                Opcode::Add,
                &field,
                &Value::NativeInt(offset),
            ));
            assert!(
                message.contains("`Pair`") && message.contains("where none of its fields starts"),
                "byte {offset}: {message}"
            );
        }
    }

    #[test]
    fn a_field_address_without_a_layout_that_numbers_its_slots_is_refused_by_name() {
        let mut module = Module::new();
        let unrecorded = module.add_type(alloc::vec![Value::Int32(0)]);
        module.bind_type_full_name(unrecorded, String::from("Unrecorded"));
        let grown = module.add_type(alloc::vec![Value::Int32(0), Value::Int32(0)]);
        module.bind_type_full_name(grown, String::from("Grown"));
        module.set_type_field_offsets(grown, 4, 4, &[0]);
        let mut vm = Vm::new();
        for (type_id, name, fields) in [(unrecorded, "`Unrecorded`", 1), (grown, "`Grown`", 2)] {
            let object = vm
                .heap_mut()
                .alloc_instance(type_id, alloc::vec![Value::Int32(0); fields]);
            let field = Value::ByRef(Location::Field { object, slot: 0 });
            let message = walk_refusal(field_pointer_arithmetic(
                Some(&module),
                &vm,
                Opcode::Add,
                &field,
                &Value::Int32(4),
            ));
            assert!(
                message.contains(name) && message.contains("no byte layout"),
                "{message}"
            );
        }
    }

    #[test]
    fn a_walk_onto_a_struct_field_or_from_a_struct_held_in_a_local_is_refused_by_name() {
        let mut module = Module::new();
        let point = || Value::Struct(alloc::vec![Value::Int32(0), Value::Int32(0)].into_boxed_slice());
        let holder = module.add_type(alloc::vec![Value::Int32(0), point()]);
        module.bind_type_full_name(holder, String::from("Holder"));
        module.set_type_field_offsets(holder, 12, 4, &[0, 4]);
        let mut vm = Vm::new();
        let object = vm
            .heap_mut()
            .alloc_instance(holder, alloc::vec![Value::Int32(1), point()]);
        let field = Value::ByRef(Location::Field { object, slot: 0 });
        let message = walk_refusal(field_pointer_arithmetic(
            Some(&module),
            &vm,
            Opcode::Add,
            &field,
            &Value::Int32(4),
        ));
        assert!(message.contains("struct-typed field"), "{message}");
        let in_local = Value::ByRef(Location::Nested {
            base: alloc::boxed::Box::new(Location::Local { frame: 0, slot: 0 }),
            slot: 0,
        });
        let message = walk_refusal(field_pointer_arithmetic(
            Some(&module),
            &vm,
            Opcode::Add,
            &in_local,
            &Value::Int32(4),
        ));
        assert!(message.contains("held in a local"), "{message}");
    }

    /// A `localloc` buffer pointer at `offset`, for the pointer-arithmetic tests.
    fn stack_pointer_at(offset: u32) -> Value {
        Value::ByRef(Location::Stack {
            frame: 0,
            buffer: 0,
            offset,
        })
    }

    #[test]
    fn checked_pointer_arithmetic_refuses_the_wrap_the_unchecked_form_allows() {
        let heap = Heap::new();
        let ovf =
            |opcode, a: &Value, b: &Value| stack_pointer_arithmetic(&heap, opcode, a, b).unwrap_err();
        let walk =
            |opcode, a: &Value, b: &Value| stack_pointer_arithmetic(&heap, opcode, a, b).unwrap();

        let all_ones = Value::NativeInt(i64::from(u32::MAX));
        assert_eq!(
            walk(Opcode::Add, &stack_pointer_at(1), &all_ones),
            Some(stack_pointer_at(0)),
        );
        assert!(matches!(
            ovf(Opcode::AddOvfUn, &stack_pointer_at(1), &all_ones),
            Trap::Overflow
        ));
        assert_eq!(
            walk(Opcode::AddOvfUn, &stack_pointer_at(1), &Value::Int32(2)),
            Some(stack_pointer_at(3)),
        );

        assert_eq!(
            walk(Opcode::Sub, &stack_pointer_at(0), &Value::Int32(1)),
            Some(stack_pointer_at(u32::MAX)),
        );
        assert!(matches!(
            ovf(Opcode::SubOvfUn, &stack_pointer_at(0), &Value::Int32(1)),
            Trap::Overflow
        ));
        assert_eq!(
            walk(Opcode::SubOvfUn, &stack_pointer_at(5), &Value::Int32(3)),
            Some(stack_pointer_at(2)),
        );

        assert_eq!(
            walk(Opcode::SubOvfUn, &stack_pointer_at(6), &stack_pointer_at(2)),
            Some(Value::NativeInt(4)),
        );
        assert_eq!(
            walk(Opcode::Sub, &stack_pointer_at(2), &stack_pointer_at(6)),
            Some(Value::NativeInt(-4)),
        );
        assert!(matches!(
            ovf(Opcode::SubOvfUn, &stack_pointer_at(2), &stack_pointer_at(6)),
            Trap::Overflow
        ));

        assert_eq!(walk(Opcode::AddOvf, &stack_pointer_at(1), &all_ones), None);
        assert_eq!(
            walk(Opcode::SubOvf, &stack_pointer_at(1), &Value::Int32(1)),
            None,
        );
        assert!(matches!(
            binary_numeric(Opcode::AddOvf, stack_pointer_at(1), all_ones).unwrap_err(),
            Trap::TypeMismatch(Opcode::AddOvf)
        ));
    }

    /// The five String-constructor anchors are five distinct identities under the scheme the
    /// recognizers actually use.
    ///
    /// The anchors have byte-identical bodies on purpose -- each is a name for the interpreter to
    /// route on and has no behavior of its own -- so a release build is free to merge them onto ONE
    /// address, and it does. While the recognizers compared function pointers, every one of them
    /// answered for whichever anchor was tested first: `new string(chars)` took the `String(char*)`
    /// path and trapped `TypeMismatch(Newobj)`, in release only, with the debug build green.
    ///
    /// This asserts the ids and not the addresses. Whether a given build merges identical functions
    /// is the optimizer's business and may change; the property this runtime needs is that the
    /// anchors stay distinguishable regardless.
    #[cfg(feature = "bcl")]
    #[test]
    fn the_five_string_ctor_anchors_are_five_distinct_ids() {
        let anchors = [
            "string_ctor_char_ptr",
            "string_ctor_char_ptr_range",
            "string_ctor_char_array",
            "string_ctor_char_array_range",
            "string_ctor_char_repeat",
        ];
        for (index, left) in anchors.iter().enumerate() {
            for right in anchors.iter().skip(index + 1) {
                assert_ne!(
                    intrinsic_id(left),
                    intrinsic_id(right),
                    "String ctor anchors {left} and {right} share a registry id"
                );
            }
        }
    }

    #[test]
    fn string_ctor_materializes_from_stackalloc_and_pinned_string() {
        let mut vm = Vm::new();
        let mut owner = Frame::new(0, MethodContext::NONE, Vec::new());
        let pointer = owner.localloc(0, 6);
        owner.write_stack(0, 0, [b'4', 0, 0, 0, 0, 0, 0, 0], 2).unwrap();
        owner.write_stack(0, 2, [b'2', 0, 0, 0, 0, 0, 0, 0], 2).unwrap();
        let frames = [owner];

        let ranged = build_string_from_pointer(&frames, &mut vm, pointer.clone(), 0, Some(2));
        let ranged = ranged.expect("range read");
        assert_eq!(
            vm.heap().as_string(ranged).as_deref(),
            Some(&[u16::from(b'4'), u16::from(b'2')][..])
        );
        let scanned = build_string_from_pointer(&frames, &mut vm, pointer.clone(), 1, None);
        let scanned = scanned.expect("terminator scan");
        assert_eq!(
            vm.heap().as_string(scanned).as_deref(),
            Some(&[u16::from(b'2')][..])
        );
        assert_eq!(
            build_string_from_pointer(&frames, &mut vm, pointer.clone(), 0, Some(4)),
            Err(Trap::NullReference)
        );
        assert_eq!(
            build_string_from_pointer(&frames, &mut vm, pointer, -1, Some(1)),
            Err(Trap::ArgumentOutOfRange(1))
        );

        let source = vm.heap_mut().alloc_text("*+");
        let tail = build_string_from_pointer(
            &frames,
            &mut vm,
            Value::ByRef(Location::StringChar {
                string: source,
                byte_offset: 2,
            }),
            0,
            None,
        )
        .expect("pinned-string tail");
        assert_eq!(vm.heap().as_string(tail).as_deref(), Some(&[43u16][..]));

        let empty = build_string_from_pointer(&frames, &mut vm, Value::NativeInt(0), 0, None)
            .expect("null pointer");
        assert_eq!(vm.heap().as_string(empty).as_deref(), Some(&[][..]));
    }

    #[test]
    fn localloc_out_of_range_store_traps() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::StindI4),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Err(Trap::NullReference));
    }

    #[test]
    fn arguments_and_locals_round_trip() {
        let body = method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::Ldarg1),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Ret),
        ]);
        let result = run_method(&body, vec![Value::Int32(40), Value::Int32(2)]);
        assert_eq!(result, Ok(Some(Value::Int32(42))));
    }

    #[test]
    fn sums_one_to_five_with_a_loop() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI40),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Stloc1),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::Ldloc1),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(Opcode::Ldloc1),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Stloc1),
            Instruction::simple(Opcode::Ldloc1),
            Instruction::simple(Opcode::LdcI45),
            Instruction::new(Opcode::BleS, Operand::Target(4)),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(15))));
    }

    #[test]
    fn compares_with_clt() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI43),
            Instruction::simple(Opcode::LdcI45),
            Instruction::simple(Opcode::Clt),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(1))));
    }

    #[test]
    fn unsigned_compare_differs_from_signed() {
        let signed = run(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(-1)),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Clt),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(signed, Ok(Some(Value::Int32(1))));
        let unsigned = run(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(-1)),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::CltUn),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(unsigned, Ok(Some(Value::Int32(0))));
    }

    #[test]
    #[cfg(feature = "float")]
    fn float_arithmetic() {
        let result = run(vec![
            Instruction::new(Opcode::LdcR8, Operand::Float64(1.5)),
            Instruction::new(Opcode::LdcR8, Operand::Float64(2.0)),
            Instruction::simple(Opcode::Mul),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Float(3.0))));
    }

    fn run_convert(opcode: Opcode, value: Operand, load: Opcode) -> Result<Option<Value>, Trap> {
        run(vec![
            Instruction::new(load, value),
            Instruction::simple(opcode),
            Instruction::simple(Opcode::Ret),
        ])
    }

    #[test]
    #[cfg(feature = "float")]
    fn conv_i4_truncates_a_float_toward_zero() {
        let result = run_convert(Opcode::ConvI4, Operand::Float64(3.9), Opcode::LdcR8);
        assert_eq!(result, Ok(Some(Value::Int32(3))));
    }

    #[test]
    fn conv_i1_sign_extends_but_conv_u1_zero_extends() {
        let signed = run_convert(Opcode::ConvI1, Operand::Int32(0x1FF), Opcode::LdcI4);
        assert_eq!(signed, Ok(Some(Value::Int32(-1))));
        let unsigned = run_convert(Opcode::ConvU1, Operand::Int32(0x1FF), Opcode::LdcI4);
        assert_eq!(unsigned, Ok(Some(Value::Int32(255))));
    }

    #[test]
    fn conv_i8_sign_extends_but_conv_u8_zero_extends_int32() {
        let signed = run_convert(Opcode::ConvI8, Operand::Int32(-1), Opcode::LdcI4);
        assert_eq!(signed, Ok(Some(Value::Int64(-1))));
        let unsigned = run_convert(Opcode::ConvU8, Operand::Int32(-1), Opcode::LdcI4);
        assert_eq!(unsigned, Ok(Some(Value::Int64(0xFFFF_FFFF))));
    }

    #[test]
    #[cfg(feature = "float")]
    fn conv_u8_from_a_float_reaches_the_top_half_of_the_ulong_range() {
        let two_pow_63 = run_convert(
            Opcode::ConvU8,
            Operand::Float64(9_223_372_036_854_775_808.0),
            Opcode::LdcR8,
        );
        assert_eq!(two_pow_63, Ok(Some(Value::Int64(i64::MIN))));
        let near_max = run_convert(
            Opcode::ConvU8,
            Operand::Float64(18_446_744_073_709_549_568.0),
            Opcode::LdcR8,
        );
        assert_eq!(
            near_max,
            Ok(Some(Value::Int64(18_446_744_073_709_549_568_u64 as i64)))
        );
        let signed = run_convert(Opcode::ConvI8, Operand::Float64(3.9), Opcode::LdcR8);
        assert_eq!(signed, Ok(Some(Value::Int64(3))));
        let small = run_convert(Opcode::ConvU8, Operand::Float64(42.7), Opcode::LdcR8);
        assert_eq!(small, Ok(Some(Value::Int64(42))));
        let minus_one = run_convert(Opcode::ConvU8, Operand::Float64(-1.0), Opcode::LdcR8);
        assert_eq!(minus_one, Ok(Some(Value::Int64(0))));
        let minus_two_point_five =
            run_convert(Opcode::ConvU8, Operand::Float64(-2.5), Opcode::LdcR8);
        assert_eq!(minus_two_point_five, Ok(Some(Value::Int64(0))));
    }

    #[test]
    #[cfg(feature = "float")]
    fn conv_r8_is_signed_but_conv_r_un_is_unsigned() {
        let signed = run_convert(Opcode::ConvR8, Operand::Int32(-1), Opcode::LdcI4);
        assert_eq!(signed, Ok(Some(Value::Float(-1.0))));
        let unsigned = run_convert(Opcode::ConvRUn, Operand::Int32(-1), Opcode::LdcI4);
        assert_eq!(unsigned, Ok(Some(Value::Float(4_294_967_295.0))));
    }

    #[test]
    fn converting_a_reference_traps() {
        let result = run(vec![
            Instruction::simple(Opcode::Ldnull),
            Instruction::simple(Opcode::ConvI4),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Err(Trap::TypeMismatch(Opcode::ConvI4)));
    }

    #[test]
    fn switch_selects_a_case_or_falls_through() {
        let code = vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::new(
                Opcode::Switch,
                Operand::Switch(vec![4, 6].into_boxed_slice()),
            ),
            Instruction::new(Opcode::LdcI4, Operand::Int32(100)),
            Instruction::simple(Opcode::Ret),
            Instruction::new(Opcode::LdcI4, Operand::Int32(10)),
            Instruction::simple(Opcode::Ret),
            Instruction::new(Opcode::LdcI4, Operand::Int32(20)),
            Instruction::simple(Opcode::Ret),
        ];
        let body = method(code);
        assert_eq!(
            run_method(&body, vec![Value::Int32(0)]),
            Ok(Some(Value::Int32(10)))
        );
        assert_eq!(
            run_method(&body, vec![Value::Int32(1)]),
            Ok(Some(Value::Int32(20)))
        );
        assert_eq!(
            run_method(&body, vec![Value::Int32(5)]),
            Ok(Some(Value::Int32(100)))
        );
    }

    #[test]
    fn starg_overwrites_an_argument() {
        let body = method(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(99)),
            Instruction::new(Opcode::StargS, Operand::Variable(0)),
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(
            run_method(&body, vec![Value::Int32(1)]),
            Ok(Some(Value::Int32(99)))
        );
    }

    #[test]
    fn stack_underflow_traps() {
        assert_eq!(
            run(vec![Instruction::simple(Opcode::Add)]),
            Err(Trap::StackUnderflow)
        );
    }

    #[test]
    fn unsupported_opcode_traps() {
        let result = run(vec![Instruction::new(
            Opcode::Newobj,
            Operand::Token(lamella_token::Token(0x0A00_0001)),
        )]);
        assert_eq!(result, Err(Trap::Unsupported(Opcode::Newobj)));
    }

    #[test]
    fn falling_off_the_end_traps() {
        assert_eq!(
            run(vec![Instruction::simple(Opcode::Nop)]),
            Err(Trap::FellThroughEnd)
        );
    }

    use crate::module::Module;
    use lamella_token::Token;

    const ADD_TOKEN: Token = Token(0x0600_0002);
    const SELF_TOKEN: Token = Token(0x0600_0003);

    #[test]
    fn static_call_adds_two_arguments() {
        let mut module = Module::new();
        let add = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            2,
        );
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::LdcI4S, Operand::Int8(40)),
                Instruction::simple(Opcode::LdcI42),
                Instruction::new(Opcode::Call, Operand::Token(ADD_TOKEN)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        module.bind_token(0, ADD_TOKEN, add);

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(42)))
        );
    }

    #[test]
    fn recursion_sums_one_to_n_across_frames() {
        let mut module = Module::new();
        let sum = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::LdcI40),
                Instruction::new(Opcode::BgtS, Operand::Target(5)),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::Ret),
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::Sub),
                Instruction::new(Opcode::Call, Operand::Token(SELF_TOKEN)),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            1,
        );
        module.bind_token(0, SELF_TOKEN, sum);

        assert_eq!(
            super::run(&module, &mut Vm::new(), sum, alloc::vec![Value::Int32(5)]),
            Ok(Some(Value::Int32(15)))
        );
    }

    #[test]
    fn an_unbound_call_token_traps() {
        let mut module = Module::new();
        let main = module.add_method_image(
            0,
            method(vec![Instruction::new(
                Opcode::Call,
                Operand::Token(ADD_TOKEN),
            )]),
            0,
        );
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Err(Trap::UnresolvedCall(ADD_TOKEN))
        );
    }

    #[test]
    fn runaway_recursion_traps_instead_of_crashing() {
        let mut module = Module::new();
        let loop_method = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Call, Operand::Token(SELF_TOKEN)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        module.bind_token(0, SELF_TOKEN, loop_method);

        assert_eq!(
            super::run(&module, &mut Vm::new(), loop_method, Vec::new()),
            Err(Trap::CallStackOverflow)
        );
    }

    #[test]
    fn session_single_steps_and_inspects_the_stack() {
        let mut module = Module::new();
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::LdcI42),
                Instruction::simple(Opcode::LdcI43),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        let mut vm = Vm::new();
        let mut session = Session::new(&module, main, Vec::new()).unwrap();

        assert_eq!(session.frame(0).unwrap().ip, 0);
        assert!(session.frame(0).unwrap().stack.is_empty());
        assert_eq!(session.step(&module, &mut vm), Ok(Status::Running));
        assert_eq!(
            session.frame(0).unwrap().stack,
            [Value::Int32(2)].as_slice()
        );
        assert_eq!(session.step(&module, &mut vm), Ok(Status::Running));
        assert_eq!(session.step(&module, &mut vm), Ok(Status::Running));
        assert_eq!(
            session.frame(0).unwrap().stack,
            [Value::Int32(5)].as_slice()
        );
        assert_eq!(
            session.step(&module, &mut vm),
            Ok(Status::Done(Some(Value::Int32(5))))
        );
    }

    #[test]
    fn set_arg_and_set_local_edit_a_live_frame() {
        let mut module = Module::new();
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::LdcI42),
                Instruction::simple(Opcode::Ret),
            ]),
            1,
        );
        let mut session = Session::new(&module, main, vec![Value::Int32(7)]).unwrap();

        assert_eq!(session.frame(0).unwrap().args, [Value::Int32(7)].as_slice());
        assert!(session.set_arg(0, 0, Value::Int32(42)));
        assert_eq!(session.frame(0).unwrap().args, [Value::Int32(42)].as_slice());

        assert!(!session.set_arg(0, 9, Value::Int32(0)));
        assert!(!session.set_arg(5, 0, Value::Int32(0)));
        assert!(!session.set_local(0, 0, Value::Int32(0)));
        assert!(!session.set_local(5, 0, Value::Int32(0)));
    }

    #[test]
    fn session_pauses_at_a_breakpoint() {
        let mut module = Module::new();
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::LdcI42),
                Instruction::simple(Opcode::LdcI43),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        let mut vm = Vm::new();
        let mut session = Session::new(&module, main, Vec::new()).unwrap();
        session.add_breakpoint(main, 2);

        assert_eq!(session.resume(&module, &mut vm), Ok(Status::Paused));
        assert_eq!(session.frame(0).unwrap().ip, 2);
        assert_eq!(
            session.frame(0).unwrap().stack,
            [Value::Int32(2), Value::Int32(3)].as_slice()
        );
        assert_eq!(session.step(&module, &mut vm), Ok(Status::Running));
        assert_eq!(
            session.resume(&module, &mut vm),
            Ok(Status::Done(Some(Value::Int32(5))))
        );
    }

    #[test]
    fn session_exposes_the_call_stack_at_a_breakpoint() {
        let mut module = Module::new();
        let add = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            2,
        );
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::LdcI42),
                Instruction::simple(Opcode::LdcI43),
                Instruction::new(Opcode::Call, Operand::Token(ADD_TOKEN)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        module.bind_token(0, ADD_TOKEN, add);
        let mut vm = Vm::new();
        let mut session = Session::new(&module, main, Vec::new()).unwrap();
        session.add_breakpoint(add, 0);

        assert_eq!(session.resume(&module, &mut vm), Ok(Status::Paused));
        assert_eq!(session.depth(), 2);
        assert_eq!(session.frame(0).unwrap().method, main);
        assert_eq!(session.frame(1).unwrap().method, add);
        assert_eq!(
            session.frame(1).unwrap().args,
            [Value::Int32(2), Value::Int32(3)].as_slice()
        );
        assert_eq!(session.step(&module, &mut vm), Ok(Status::Running));
        assert_eq!(
            session.resume(&module, &mut vm),
            Ok(Status::Done(Some(Value::Int32(5))))
        );
    }

    #[test]
    fn newobj_constructs_then_instance_calls_read_and_write_a_field() {
        let count_field = Token(0x0400_0001);
        let ctor_token = Token(0x0600_0010);
        let inc_token = Token(0x0600_0011);
        let get_token = Token(0x0600_0012);

        let mut module = Module::new();
        let counter = module.add_type(vec![Value::Int32(0)]);
        module.bind_field(0, count_field, 0);

        let ctor = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::new(Opcode::Stfld, Operand::Token(count_field)),
                Instruction::simple(Opcode::Ret),
            ]),
            2,
        );
        module.set_method_type(ctor, counter);
        module.bind_token(0, ctor_token, ctor);

        let inc = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg0),
                Instruction::new(Opcode::Ldfld, Operand::Token(count_field)),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::Add),
                Instruction::new(Opcode::Stfld, Operand::Token(count_field)),
                Instruction::simple(Opcode::Ret),
            ]),
            1,
        );
        module.set_method_type(inc, counter);
        module.bind_token(0, inc_token, inc);

        let get = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::new(Opcode::Ldfld, Operand::Token(count_field)),
                Instruction::simple(Opcode::Ret),
            ]),
            1,
        );
        module.set_method_type(get, counter);
        module.bind_token(0, get_token, get);

        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::LdcI4S, Operand::Int8(10)),
                Instruction::new(Opcode::Newobj, Operand::Token(ctor_token)),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::new(Opcode::Call, Operand::Token(inc_token)),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::new(Opcode::Call, Operand::Token(inc_token)),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::new(Opcode::Call, Operand::Token(get_token)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(12)))
        );
    }

    #[test]
    fn arrays_allocate_store_load_and_measure_length() {
        let elem = Token(0x0100_0005);
        let mut module = Module::new();
        module.bind_array_default(0, elem, Value::Int32(0));

        let store = |index: Opcode, value: i8| {
            [
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(index),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(value)),
                Instruction::simple(Opcode::StelemI4),
            ]
        };
        let load = |index: Opcode| {
            [
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(index),
                Instruction::simple(Opcode::LdelemI4),
            ]
        };
        let mut code = alloc::vec![
            Instruction::simple(Opcode::LdcI43),
            Instruction::new(Opcode::Newarr, Operand::Token(elem)),
            Instruction::simple(Opcode::Stloc0),
        ];
        code.extend(store(Opcode::LdcI40, 10));
        code.extend(store(Opcode::LdcI41, 20));
        code.extend(store(Opcode::LdcI42, 30));
        code.extend(load(Opcode::LdcI40));
        code.extend(load(Opcode::LdcI41));
        code.push(Instruction::simple(Opcode::Add));
        code.extend(load(Opcode::LdcI42));
        code.push(Instruction::simple(Opcode::Add));
        code.extend([
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::Ldlen),
            Instruction::simple(Opcode::ConvI4),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Ret),
        ]);
        let main = module.add_method_image(0, method(code), 0);

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(63)))
        );
    }

    #[test]
    fn resolved_array_targets_without_cast_records_enforce_named_store_checks() {
        for frozen in [false, true] {
            let mut module = Module::new();
            let unrelated = Token(0x0200_0001);
            let array_base = Token(0x0200_0002);
            for token in [unrelated, array_base] {
                let type_id = module.add_type(Vec::new());
                module.bind_type_token(0, token, type_id);
            }
            module.mark_array_base_name(0, array_base);
            if frozen { module.freeze(); }
            for (token, allowed) in [(unrelated, false), (array_base, true)] {
                let handle = asm_key(0, token.0);
                assert_eq!(module.cast_elem(handle), None);
                for rectangular in [false, true] {
                    let mut vm = Vm::new();
                    let array = crate::intrinsics::array_create_instance(
                        &mut vm, &module, &[Value::NativeInt(handle as i64), Value::Int32(1)],
                    ).unwrap().unwrap();
                    let Value::Object(destination) = array else { panic!("array") };
                    let source = if rectangular {
                        vm.heap_mut().alloc_md_array(vec![1, 1])
                    } else {
                        vm.heap_mut().alloc_array(vec![Value::Int32(0)])
                    };
                    let mut frame = Frame::new(0, MethodContext::NONE, Vec::new());
                    frame.stack.extend([array, Value::Int32(0), Value::Object(source)]);
                    let result = step(&mut frame, 0, &[], 1, Some(&module), &mut vm,
                        &Instruction::simple(Opcode::StelemRef));
                    if allowed {
                        assert!(result.is_ok(), "{:?}", result.err());
                        assert_eq!(vm.heap().array_get(destination, 0), Some(Value::Object(source)));
                    } else {
                        assert!(matches!(result, Err(Trap::ArrayTypeMismatch)),
                            "a resolved target without a cast record must reject unrelated arrays: {:?}", result.err());
                        assert_eq!(vm.heap().array_get(destination, 0), Some(Value::Null));
                    }
                }
            }
        }
    }

    #[test]
    fn packed_byte_array_matches_boxed_element_semantics() {
        let elem = Token(0x0100_0005);
        let mut module = Module::new();
        module.bind_array_default(0, elem, Value::Int32(0));
        module.bind_array_prim_kind(0, elem, crate::object::PrimKind::U1);

        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::LdcI43),
                Instruction::new(Opcode::Newarr, Operand::Token(elem)),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::LdcI40),
                Instruction::new(Opcode::LdcI4, Operand::Int32(511)),
                Instruction::simple(Opcode::StelemI1),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::LdcI41),
                Instruction::new(Opcode::LdcI4, Operand::Int32(200)),
                Instruction::simple(Opcode::StelemI1),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::LdelemU1),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::LdelemU1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(455)))
        );
    }

    fn generic_array_module(name: &str, zero: Value) -> (Module, Token, TypeId) {
        let definition = Token::new(0x02, 3);
        let synthetic = Token::new(0x7E, 1);
        let mut module = Module::new();
        let element = module.add_type(Vec::new());
        module.set_type_is_value_type(element, true);
        module.bind_type_token(1, definition, element);
        module.bind_type_full_name(element, name.into());
        module.set_type_size(1, definition, 0);
        module.bind_type_token(0, synthetic, element);
        module.bind_array_default(0, synthetic, zero);
        (module, synthetic, element)
    }

    fn prepared(mut module: Module, preparation: &str) -> Option<Module> {
        match preparation {
            "frozen" => module.freeze(),
            "baked" => {
                #[cfg(not(feature = "code-in-place"))]
                return None;
                #[cfg(feature = "code-in-place")]
                {
                    let image = module.write_baked(None).expect("bake the generic array's module");
                    module = Module::from_baked(Box::leak(image.into_boxed_slice()))
                        .expect("read the generic array's module").0;
                }
            }
            _ => {}
        }
        Some(module)
    }

    #[test]
    fn a_generic_array_of_int_reads_its_elements_as_ints() {
        for preparation in ["live", "frozen", "baked"] {
            let (mut module, synthetic, _) = generic_array_module("System.Int32", Value::Int32(0));
            let main = module.add_method_image(
                0,
                method(vec![
                    Instruction::simple(Opcode::LdcI42),
                    Instruction::new(Opcode::Newarr, Operand::Token(synthetic)),
                    Instruction::simple(Opcode::Stloc0),
                    Instruction::simple(Opcode::Ldloc0),
                    Instruction::simple(Opcode::LdcI40),
                    Instruction::new(Opcode::LdcI4S, Operand::Int8(7)),
                    Instruction::new(Opcode::Stelem, Operand::Token(synthetic)),
                    Instruction::simple(Opcode::Ldloc0),
                    Instruction::simple(Opcode::LdcI40),
                    Instruction::new(Opcode::Ldelema, Operand::Token(synthetic)),
                    Instruction::simple(Opcode::LdindI4),
                    Instruction::simple(Opcode::Ldloc0),
                    Instruction::simple(Opcode::LdcI41),
                    Instruction::new(Opcode::Ldelema, Operand::Token(synthetic)),
                    Instruction::simple(Opcode::LdindI4),
                    Instruction::simple(Opcode::Add),
                    Instruction::simple(Opcode::Ret),
                ]),
                0,
            );
            let Some(module) = prepared(module, preparation) else { continue };
            assert_eq!(
                super::run(&module, &mut Vm::new(), main, Vec::new()),
                Ok(Some(Value::Int32(7))),
                "{preparation}"
            );
        }
    }

    #[test]
    fn a_generic_array_of_intptr_reads_its_elements_as_native_ints() {
        for preparation in ["live", "frozen", "baked"] {
            let (mut module, synthetic, _) = generic_array_module("System.IntPtr", Value::NativeInt(0));
            let main = module.add_method_image(
                0,
                method(vec![
                    Instruction::simple(Opcode::LdcI42),
                    Instruction::new(Opcode::Newarr, Operand::Token(synthetic)),
                    Instruction::simple(Opcode::Stloc0),
                    Instruction::simple(Opcode::Ldloc0),
                    Instruction::simple(Opcode::LdcI40),
                    Instruction::new(Opcode::LdcI4S, Operand::Int8(6)),
                    Instruction::simple(Opcode::ConvI),
                    Instruction::new(Opcode::Stelem, Operand::Token(synthetic)),
                    Instruction::simple(Opcode::Ldloc0),
                    Instruction::simple(Opcode::LdcI40),
                    Instruction::new(Opcode::Ldelema, Operand::Token(synthetic)),
                    Instruction::simple(Opcode::LdindI),
                    Instruction::simple(Opcode::Ldloc0),
                    Instruction::simple(Opcode::LdcI41),
                    Instruction::new(Opcode::Ldelema, Operand::Token(synthetic)),
                    Instruction::simple(Opcode::LdindI),
                    Instruction::simple(Opcode::Add),
                    Instruction::simple(Opcode::Ret),
                ]),
                0,
            );
            let Some(module) = prepared(module, preparation) else { continue };
            assert_eq!(
                super::run(&module, &mut Vm::new(), main, Vec::new()),
                Ok(Some(Value::NativeInt(6))),
                "{preparation}"
            );
        }
    }

    fn array_address_module() -> (Module, Token, Token) {
        let root_token = Token(0x0200_0001);
        let leaf_token = Token(0x0200_0002);
        let mut module = Module::new();
        let root = module.add_type(Vec::new());
        let leaf = module.add_type(Vec::new());
        module.set_type_base(leaf, Some(root));
        module.bind_type_token(0, root_token, root);
        module.bind_type_token(0, leaf_token, leaf);
        module.bind_type_token(1, Token(0x0100_0001), root);
        (module, root_token, leaf_token)
    }

    #[test]
    #[cfg(feature = "exceptions")]
    fn ldelema_rejects_covariant_arguments_before_entering_the_callee() {
        let (mut module, root, leaf) = array_address_module();
        let field = Token(0x0400_0001);
        let callee_token = Token(0x0600_0001);
        module.bind_static_field(0, field, Value::Int32(0));
        let callee = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::LdcI41),
            Instruction::new(Opcode::Stsfld, Operand::Token(field)),
            ret(),
        ]), 1);
        module.bind_token(0, callee_token, callee);
        let main = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::LdcI40),
            Instruction::new(Opcode::Ldelema, Operand::Token(root)),
            Instruction::new(Opcode::Call, Operand::Token(callee_token)),
            ret(),
        ]), 1);
        for (element, should_call) in [(root, true), (leaf, false)] {
            let mut vm = Vm::new();
            vm.init_statics(&module.decode_static_defaults());
            let array = vm.heap_mut().alloc_typed_array(vec![Value::Null], asm_key(0, element.0));
            let result = super::run(&module, &mut vm, main, vec![Value::Object(array)]);
            if should_call {
                assert_eq!(result, Ok(None));
            } else {
                assert!(matches!(result, Err(Trap::UnhandledException(_))), "{result:?}");
            }
            assert_eq!(vm.static_field(0), Some(Value::Int32(i32::from(should_call))));
        }
    }

    #[test]
    fn ldelema_checks_reference_identity_and_preserves_primitive_storage_types() {
        let (mut module, root, leaf) = array_address_module();
        let string = Token(0x0100_0002);
        let object = Token(0x0100_0003);
        let int = Token(0x0100_0004);
        let uint = Token(0x0100_0005);
        let long = Token(0x0100_0006);
        module.bind_cast_elem(0, string, CastElem::String);
        module.bind_cast_elem(0, object, CastElem::Object);
        module.bind_cast_elem(0, int, CastElem::Prim(CastPrim::I4));
        module.bind_cast_elem(0, uint, CastElem::Prim(CastPrim::U4));
        module.bind_cast_elem(0, long, CastElem::Prim(CastPrim::I8));
        for (element, requested, allowed) in [
            (asm_key(0, root.0), root, true),
            (asm_key(1, 0x0100_0001), root, true),
            (asm_key(0, leaf.0), root, false),
            (asm_key(0, string.0), string, true),
            (asm_key(0, string.0), object, false),
            (asm_key(0, uint.0), int, true),
            (asm_key(0, long.0), int, false),
        ] {
            let mut vm = Vm::new();
            let array = vm.heap_mut().alloc_typed_array(vec![Value::Null], element);
            let mut frame = Frame::new(0, MethodContext::NONE, Vec::new());
            frame.stack.extend([Value::Object(array), Value::Int32(0)]);
            let result = step(&mut frame, 0, &[], 1, Some(&module), &mut vm,
                &Instruction::new(Opcode::Ldelema, Operand::Token(requested)));
            if allowed {
                assert!(result.is_ok(), "{element:#x} -> {requested:?}: {:?}", result.err());
                assert_eq!(frame.pop().unwrap(), Value::ByRef(Location::Element {
                    array, index: 0, byte_offset: 0,
                }));
            } else {
                assert!(matches!(result, Err(Trap::ArrayTypeMismatch)),
                    "{element:#x} -> {requested:?}: {:?}", result.err());
            }
        }
    }

    #[test]
    fn ldelema_requires_exact_nested_array_types() {
        let mut failures = Vec::new();
        for reflected in [false, true] {
            for mode in 0..=2 {
                if mode == 2 && !cfg!(feature = "code-in-place") { continue; }
                let mut module = Module::new();
                let enum_a = Token(0x0200_0001);
                let enum_b = Token(0x0200_0002);
                let enum_ref = Token(0x0100_0001);
                for (token, name) in [(enum_a, "Sample.A"), (enum_b, "Sample.B")] {
                    let id = module.add_type(Vec::new());
                    module.bind_type_token(0, token, id);
                    if token == enum_a { module.bind_type_token(1, enum_ref, id); }
                    module.set_enum_constant(0, token.0, 0, String::from("Zero"));
                    module.set_enum_width(0, token.0, 4);
                    if reflected {
                        module.bind_reflect_type(asm_key(0, token.0), crate::module::ReflectType {
                            full_name: String::from(name), is_value_type: true,
                            ..crate::module::ReflectType::default()
                        });
                    }
                }
                let int = CastElem::Prim(CastPrim::I4);
                let uint = CastElem::Prim(CastPrim::U4);
                let a = CastElem::Named(asm_key(0, enum_a.0));
                let b = CastElem::Named(asm_key(0, enum_b.0));
                let a_ref = CastElem::Named(asm_key(1, enum_ref.0));
                let vector = |elem| CastElem::Array(Box::new(elem));
                let rect = |elem, rank| CastElem::MdArray { element: Box::new(elem), rank };
                let cases = [
                    (int.clone(), uint.clone(), true),
                    (a.clone(), int.clone(), true),
                    (a.clone(), b.clone(), true),
                    (vector(int.clone()), vector(int.clone()), true),
                    (vector(int.clone()), vector(uint.clone()), false),
                    (vector(uint.clone()), vector(int.clone()), false),
                    (vector(a.clone()), vector(int.clone()), false),
                    (vector(int.clone()), vector(a.clone()), false),
                    (vector(a.clone()), vector(b.clone()), false),
                    (vector(a.clone()), vector(a_ref.clone()), true),
                    (vector(CastElem::String), vector(CastElem::Object), false),
                    (vector(vector(int.clone())), vector(vector(uint.clone())), false),
                    (vector(vector(a.clone())), vector(vector(a_ref)), true),
                    (rect(int.clone(), 1), rect(uint.clone(), 1), false),
                    (rect(int.clone(), 1), rect(int.clone(), 2), false),
                    (rect(int.clone(), 1), vector(int.clone()), false),
                    (rect(int.clone(), 2), rect(int.clone(), 2), true),
                    (rect(int.clone(), 2), rect(uint.clone(), 2), false),
                    (rect(a, 2), rect(b, 2), false),
                    (vector(rect(int, 2)), vector(rect(uint, 2)), false),
                ];
                for (i, (held, wanted, _)) in cases.iter().enumerate() {
                    let token = Token(0x1b00_0001 + i as u32);
                    module.bind_cast_elem(0, token, held.clone());
                    module.bind_cast_elem(1, token, wanted.clone());
                }
                if mode == 1 { module.freeze(); }
                #[cfg(feature = "code-in-place")]
                if mode == 2 {
                    let bytes = module.write_baked(None).unwrap();
                    module = Module::from_baked(Box::leak(bytes.into_boxed_slice())).unwrap().0;
                }
                for (i, (held, wanted, allowed)) in cases.iter().enumerate() {
                    let token = Token(0x1b00_0001 + i as u32);
                    let mut vm = Vm::new();
                    let array = vm.heap_mut().alloc_typed_array(vec![Value::Null], asm_key(0, token.0));
                    let mut frame = Frame::new(0, MethodContext { asm: 1, ..MethodContext::NONE }, Vec::new());
                    frame.stack.extend([Value::Object(array), Value::Int32(0)]);
                    let result = step(&mut frame, 0, &[], 1, Some(&module), &mut vm,
                        &Instruction::new(Opcode::Ldelema, Operand::Token(token)));
                    let matched = if *allowed {
                        result.is_ok() && frame.pop() == Ok(Value::ByRef(Location::Element {
                            array, index: 0, byte_offset: 0,
                        }))
                    } else {
                        matches!(result, Err(Trap::ArrayTypeMismatch))
                    };
                    if !matched {
                        failures.push(format!("{held:?} -> {wanted:?}, allowed={allowed}, \
                            reflected={reflected}, storage={mode}: {:?}", result.err()));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn ldelema_readonly_skips_the_type_check_for_one_address_only() {
        let (module, root, leaf) = array_address_module();
        let mut vm = Vm::new();
        let array = vm.heap_mut().alloc_typed_array(vec![Value::Null], asm_key(0, leaf.0));
        let mut frame = Frame::new(0, MethodContext::NONE, Vec::new());
        step(&mut frame, 0, &[], 1, Some(&module), &mut vm,
            &Instruction::simple(Opcode::Readonly)).unwrap();
        frame.stack.extend([Value::Object(array), Value::Int32(0)]);
        let address = Instruction::new(Opcode::Ldelema, Operand::Token(root));
        step(&mut frame, 0, &[], 1, Some(&module), &mut vm, &address).unwrap();
        assert_eq!(frame.pop().unwrap(), Value::ByRef(Location::Element {
            array, index: 0, byte_offset: 0,
        }));
        frame.stack.extend([Value::Object(array), Value::Int32(0)]);
        assert!(matches!(step(&mut frame, 0, &[], 1, Some(&module), &mut vm, &address),
            Err(Trap::ArrayTypeMismatch)));
    }

    #[test]
    #[cfg(feature = "exceptions")]
    fn readonly_array_address_call_does_not_change_a_later_writable_address() {
        let (mut module, root, leaf) = array_address_module();
        let address_token = Token(0x0A00_0001);
        let address = module.add_intrinsic(
            0,
            crate::intrinsics::md_array_address,
            crate::intrinsic_registry::intrinsic_id("md_array_address"),
            3,
        );
        module.bind_token(0, address_token, address);
        let main = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::LdcI40),
            Instruction::simple(Opcode::LdcI40),
            Instruction::simple(Opcode::Readonly),
            Instruction::new(Opcode::Call, Operand::Token(address_token)),
            Instruction::simple(Opcode::Pop),
            Instruction::simple(Opcode::Ldarg1),
            Instruction::simple(Opcode::LdcI40),
            Instruction::new(Opcode::Ldelema, Operand::Token(root)),
            Instruction::simple(Opcode::Pop),
            ret(),
        ]), 2);
        for (element, allowed) in [(root, true), (leaf, false)] {
            let mut vm = Vm::new();
            let grid = vm.heap_mut().alloc_md_array(vec![1, 1]);
            let vector = vm.heap_mut().alloc_typed_array(vec![Value::Null], asm_key(0, element.0));
            let result = super::run(&module, &mut vm, main,
                vec![Value::Object(grid), Value::Object(vector)]);
            if allowed {
                assert_eq!(result, Ok(None));
            } else {
                assert!(matches!(result, Err(Trap::UnhandledException(_))), "{result:?}");
            }
        }
    }

    #[test]
    fn pinned_byte_pointer_walk_steps_by_the_true_element_width() {
        let elem = Token(0x0100_0005);
        let mut module = Module::new();
        module.bind_array_default(0, elem, Value::Int32(0));
        module.bind_array_prim_kind(0, elem, crate::object::PrimKind::U1);

        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::LdcI44),
                Instruction::new(Opcode::Newarr, Operand::Token(elem)),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::LdcI43),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(42)),
                Instruction::simple(Opcode::StelemI1),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::LdcI40),
                Instruction::new(Opcode::Ldelema, Operand::Token(elem)),
                Instruction::simple(Opcode::LdcI43),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::LdindU1),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(42)))
        );
    }

    #[test]
    #[cfg(feature = "exceptions")]
    fn array_index_out_of_range_throws() {
        let elem = Token(0x0100_0005);
        let mut module = Module::new();
        module.bind_array_default(0, elem, Value::Int32(0));
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::LdcI42),
                Instruction::new(Opcode::Newarr, Operand::Token(elem)),
                Instruction::simple(Opcode::LdcI45),
                Instruction::simple(Opcode::LdelemI4),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        let escaped = super::run(&module, &mut Vm::new(), main, Vec::new());
        let Err(Trap::UnhandledException(_detail)) = escaped else {
            panic!("expected an unhandled exception, got {escaped:?}");
        };
    }

    #[test]
    fn callvirt_dispatches_on_the_runtime_type() {
        let ctor_token = Token(0x0600_0020);
        let speak_token = Token(0x0600_0021);

        let mut module = Module::new();
        let base = module.add_type(vec![]);
        let derived = module.add_type(vec![]);

        let base_speak = module.add_method_image(
            0,
            method(vec![Instruction::simple(Opcode::LdcI41), ret()]),
            1,
        );
        module.set_method_type(base_speak, base);
        let derived_speak = module.add_method_image(
            0,
            method(vec![Instruction::simple(Opcode::LdcI42), ret()]),
            1,
        );
        module.set_method_type(derived_speak, derived);

        module.set_vtable(base, vec![base_speak]);
        module.set_vtable(derived, vec![derived_speak]);
        module.bind_method_slot(base_speak, 0);
        module.bind_method_slot(derived_speak, 0);

        let ctor = module.add_method_image(0, method(vec![ret()]), 1);
        module.set_method_type(ctor, derived);
        module.bind_token(0, ctor_token, ctor);
        module.bind_token(0, speak_token, base_speak);

        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Newobj, Operand::Token(ctor_token)),
                Instruction::new(Opcode::Callvirt, Operand::Token(speak_token)),
                ret(),
            ]),
            0,
        );

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(2)))
        );
    }

    fn constrained_reference_program(boxed: bool) -> (Module, MethodId, TypeId) {
        let constraint = Token(0x0200_0001);
        let derived_token = Token(0x0200_0002);
        let speak = Token(0x0600_0001);
        let ran = Token(0x0400_0001);
        let mut module = Module::new();
        let base = module.add_type(vec![]);
        let derived = module.add_type(vec![]);
        module.bind_type_token(0, constraint, base);
        module.bind_type_token(0, derived_token, derived);
        module.set_type_base(derived, Some(base));
        module.set_type_is_value_type(derived, boxed);
        module.bind_static_field(0, ran, Value::Int32(0));
        let base_speak = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::LdcI41),
            Instruction::new(Opcode::Stsfld, Operand::Token(ran)),
            Instruction::simple(Opcode::LdcI41),
            ret(),
        ]), 1);
        let derived_speak = module.add_method_image(0, method(if boxed {
            vec![Instruction::simple(Opcode::Ldarg0), Instruction::simple(Opcode::LdindI4), ret()]
        } else {
            vec![Instruction::simple(Opcode::LdcI42), ret()]
        }), 1);
        module.set_method_type(base_speak, base);
        module.set_method_type(derived_speak, derived);
        module.set_vtable(base, vec![base_speak]);
        module.set_vtable(derived, vec![derived_speak]);
        module.bind_method_slot(base_speak, 0);
        module.bind_method_slot(derived_speak, 0);
        module.bind_token(0, speak, base_speak);
        let main = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::new(Opcode::Constrained, Operand::Token(constraint)),
            Instruction::new(Opcode::Callvirt, Operand::Token(speak)),
            ret(),
        ]), 1);
        (module, main, derived)
    }

    #[test]
    #[cfg(feature = "exceptions")]
    fn constrained_reference_null_throws_before_the_method_runs() {
        let (module, main, _) = constrained_reference_program(false);
        let mut vm = Vm::new();
        vm.init_statics(&[Value::Int32(0)]);
        let result = super::run(&module, &mut vm, main, vec![Value::Null]);
        let Err(Trap::UnhandledException(detail)) = result else {
            panic!("expected NullReferenceException, got {result:?}");
        };
        assert_eq!(detail.tag, crate::exception::exception_tag("System.NullReferenceException"));
        assert_eq!(vm.static_field(0), Some(Value::Int32(0)));
    }

    #[test]
    fn constrained_reference_dispatches_on_the_referenced_object() {
        let (module, main, derived) = constrained_reference_program(false);
        let mut vm = Vm::new();
        let receiver = vm.heap_mut().alloc_instance(derived, vec![]);
        assert_eq!(super::run(&module, &mut vm, main, vec![Value::Object(receiver)]),
            Ok(Some(Value::Int32(2))));
    }

    #[test]
    fn constrained_reference_dispatches_and_unboxes_a_boxed_value() {
        let (module, main, _) = constrained_reference_program(true);
        let mut vm = Vm::new();
        let receiver = vm.heap_mut().alloc_boxed(asm_key(0, 0x0200_0002), Value::Int32(42));
        assert_eq!(super::run(&module, &mut vm, main, vec![Value::Object(receiver)]),
            Ok(Some(Value::Int32(42))));
    }

    /// Runs `ldarga.s 0; <load>; ret` on `argument`: the CIL of `*(T*)&argument` (ECMA-334 2nd ed 25.4,
    /// 25.5.1 and 25.5.4; ECMA-335 6th ed III.3.42).
    fn load_through_argument_address(load: Opcode, argument: Value) -> Result<Option<Value>, Trap> {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::simple(load),
            ret(),
        ]), 1);
        super::run(&module, &mut Vm::new(), main, vec![argument])
    }

    #[test]
    #[cfg(feature = "float")]
    fn a_float_read_through_an_int_address_is_the_ints_bits() {
        assert_eq!(load_through_argument_address(Opcode::LdindR4, Value::Int32(0x3F80_0000)),
            Ok(Some(Value::Single(1.0))));
        let Ok(Some(Value::Single(nan))) = load_through_argument_address(Opcode::LdindR4, Value::Int32(0x7FC0_0000))
        else {
            panic!("expected a float32");
        };
        assert!(nan.is_nan());
    }

    #[test]
    #[cfg(feature = "float")]
    fn an_int_read_through_a_float_address_is_the_floats_bits() {
        assert_eq!(load_through_argument_address(Opcode::LdindU4, Value::Single(1.0)),
            Ok(Some(Value::Int32(0x3F80_0000))));
        assert_eq!(load_through_argument_address(Opcode::LdindI4, Value::Single(-0.0)),
            Ok(Some(Value::Int32(i32::MIN))));
    }

    #[test]
    #[cfg(feature = "float")]
    fn a_double_and_a_long_read_each_others_bits() {
        assert_eq!(load_through_argument_address(Opcode::LdindR8, Value::Int64(0x3FF0_0000_0000_0000)),
            Ok(Some(Value::Float(1.0))));
        assert_eq!(load_through_argument_address(Opcode::LdindI8, Value::Float(-2.0)),
            Ok(Some(Value::Int64(0xC000_0000_0000_0000_u64 as i64))));
    }

    #[test]
    fn a_scalar_read_of_a_struct_reference_or_pointer_is_refused() {
        let s = Value::Struct(vec![Value::Int32(42)].into_boxed_slice());
        assert_eq!(load_through_argument_address(Opcode::LdindI4, s.clone()), Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(load_through_argument_address(Opcode::LdindU1, s.clone()), Err(Trap::Unsupported(Opcode::LdindU1)));
        let pointer = Value::ByRef(Location::Arg { frame: 0, slot: 0 });
        assert_eq!(load_through_argument_address(Opcode::LdindU1, pointer.clone()), Err(Trap::Unsupported(Opcode::LdindU1)));
        assert_eq!(load_through_argument_address(Opcode::LdindRef, Value::Int32(1)), Err(Trap::Unsupported(Opcode::LdindRef)));
        let mut module = Module::new();
        let ldobj = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::new(Opcode::Ldobj, Operand::Token(Token(0x0200_0001))),
            ret(),
        ]), 1);
        assert_eq!(super::run(&module, &mut Vm::new(), ldobj, vec![s.clone()]), Ok(Some(s)));
        assert_eq!(load_through_argument_address(Opcode::LdindRef, Value::Null), Ok(Some(Value::Null)));
        assert_eq!(load_through_argument_address(Opcode::LdindI, pointer.clone()), Ok(Some(pointer)));
    }

    #[test]
    fn a_pointer_read_as_an_object_reference_is_its_own_value() {
        let pointer = Value::ByRef(Location::Arg { frame: 0, slot: 0 });
        assert_eq!(load_through_argument_address(Opcode::LdindRef, pointer.clone()), Ok(Some(pointer)));
        assert_eq!(load_through_argument_address(Opcode::LdindRef, Value::NativeInt(0x1234)),
            Ok(Some(Value::NativeInt(0x1234))));
    }

    #[test]
    #[cfg(not(feature = "float"))]
    fn a_floating_read_without_float_support_is_refused() {
        assert_eq!(load_through_argument_address(Opcode::LdindR4, Value::Int32(0x3F80_0000)),
            Err(Trap::Unsupported(Opcode::LdindR4)));
    }

    #[test]
    fn a_read_of_the_slots_own_type_is_unchanged() {
        assert_eq!(load_through_argument_address(Opcode::LdindI4, Value::Int32(-7)), Ok(Some(Value::Int32(-7))));
        assert_eq!(load_through_argument_address(Opcode::LdindI8, Value::Int64(-7)), Ok(Some(Value::Int64(-7))));
        assert_eq!(load_through_argument_address(Opcode::LdindU1, Value::Int32(0x1FF)), Ok(Some(Value::Int32(0xFF))));
    }

    /// Runs `ldarg.0` for an object `o`, or `ldarga.s 0` for a struct argument `s`, then `ldflda f; <load>;
    /// ret`: the read of `*(T*)p` once `p` holds the field's address, which C# takes inside `fixed (U* p =
    /// &o.f)` for an object's field, a moveable variable, and as `&s.f` for a struct argument's, a fixed
    /// one (ECMA-334 2nd ed 25.3, 25.4 and 25.6); `ldflda` loads the field's address (ECMA-335 6th ed
    /// III.4.11) and the `ldind` reads through it (III.3.42).
    fn load_through_field_address(load: Opcode, receiver: Value, field_value: Value) -> Result<Option<Value>, Trap> {
        let field = Token(0x0400_0001);
        let mut module = Module::new();
        let class = module.add_type(vec![field_value.clone()]);
        module.bind_field(0, field, 0);
        module.bind_field_type(0, field, class);
        let first = if matches!(receiver, Value::Struct(_)) {
            Instruction::new(Opcode::LdargaS, Operand::Variable(0))
        } else {
            Instruction::simple(Opcode::Ldarg0)
        };
        let main = module.add_method_image(0, method(vec![
            first,
            Instruction::new(Opcode::Ldflda, Operand::Token(field)),
            Instruction::simple(load),
            ret(),
        ]), 1);
        let mut vm = Vm::new();
        let receiver = match receiver {
            Value::Struct(_) => receiver,
            _ => Value::Object(vm.heap_mut().alloc_instance(class, vec![field_value])),
        };
        super::run(&module, &mut vm, main, vec![receiver])
    }

    /// Runs `ldsflda s; <load>; ret` on a static holding `value`: the read of `*(T*)p` once `p` holds the
    /// static's address, which C# takes inside `fixed (U* p = &s)`, a static field being a moveable
    /// variable (ECMA-334 2nd ed 25.3, 25.4 and 25.6); `ldsflda` loads the static's address (ECMA-335 6th
    /// ed III.4.15) and the `ldind` reads through it (III.3.42).
    fn load_through_static_address(load: Opcode, value: Value) -> Result<Option<Value>, Trap> {
        let field = Token(0x0400_0001);
        let mut module = Module::new();
        module.bind_static_field(0, field, value);
        let main = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::Ldsflda, Operand::Token(field)),
            Instruction::simple(load),
            ret(),
        ]), 0);
        super::run(&module, &mut Vm::new(), main, Vec::new())
    }

    /// Runs `ldarg.0; <load>; ret` on a pointer to element `[0, 0]` of a 1-by-1 rectangular array
    /// holding `value`: the read of `*(T*)p` after `fixed (U* p = &a[0, 0])` (ECMA-334 2nd ed 25.4 and
    /// 25.6), the element's address being what the array's `Address` method returns (ECMA-335 6th ed
    /// II.14.2) and the `ldind` reading through it (III.3.42).
    fn load_from_rectangular(load: Opcode, value: Value) -> Result<Option<Value>, Trap> {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(load),
            ret(),
        ]), 1);
        let mut vm = Vm::new();
        let array = vm.heap_mut().alloc_md_array(vec![1, 1]);
        assert!(vm.heap_mut().md_array_set(array, &[0, 0], value));
        super::run(&module, &mut vm, main, vec![Value::ByRef(Location::Element { array, index: 0, byte_offset: 0 })])
    }

    #[test]
    #[cfg(feature = "float")]
    fn a_number_is_read_as_another_kind_only_at_a_variables_own_address() {
        assert_eq!(load_through_argument_address(Opcode::LdindR4, Value::Int32(0x3F80_0000)), Ok(Some(Value::Single(1.0))));
        assert_eq!(load_through_argument_address(Opcode::LdindU4, Value::Single(1.0)), Ok(Some(Value::Int32(0x3F80_0000))));
        assert_eq!(load_through_argument_address(Opcode::LdindR8, Value::Int64(0x3FF0_0000_0000_0000)), Ok(Some(Value::Float(1.0))));
        assert_eq!(load_through_argument_address(Opcode::LdindI8, Value::Float(1.0)), Ok(Some(Value::Int64(0x3FF0_0000_0000_0000))));
        let float_one = Value::Single(1.0);
        assert_eq!(load_through_field_address(Opcode::LdindI4, Value::Null, float_one.clone()), Err(Trap::Unsupported(Opcode::LdindI4)));
        let structure = Value::Struct(vec![float_one.clone()].into_boxed_slice());
        assert_eq!(load_through_field_address(Opcode::LdindI4, structure, float_one.clone()), Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(load_through_static_address(Opcode::LdindI4, float_one.clone()), Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(load_inside_element(Opcode::LdindI4, vec![float_one.clone()], 4, 0), Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(load_from_rectangular(Opcode::LdindI4, float_one.clone()), Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(load_from_rectangular(Opcode::LdindR4, Value::Int32(0x3F80_0000)), Err(Trap::Unsupported(Opcode::LdindR4)));
        assert_eq!(load_through_static_address(Opcode::LdindR8, Value::Int64(0x3FF0_0000_0000_0000)), Err(Trap::Unsupported(Opcode::LdindR8)));
        assert_eq!(load_from_packed(Opcode::LdindU4, crate::PrimKind::F4, vec![0, 0, 0x80, 0x3F], 0), Ok(Some(Value::Int32(0x3F80_0000))));
        assert_eq!(load_from_packed(Opcode::LdindR4, crate::PrimKind::I4, vec![0, 0, 0x80, 0x3F], 0), Ok(Some(Value::Single(1.0))));
        assert_eq!(load_through_field_address(Opcode::LdindR4, Value::Null, float_one.clone()), Ok(Some(float_one.clone())));
        assert_eq!(load_through_static_address(Opcode::LdindU4, Value::Int32(-1)), Ok(Some(Value::Int32(-1))));
        assert_eq!(load_from_rectangular(Opcode::LdindR4, float_one.clone()), Ok(Some(float_one)));
        assert_eq!(load_through_argument_address(Opcode::LdindR8, Value::NativeInt(0x3FF0_0000_0000_0000)),
            Err(Trap::Unsupported(Opcode::LdindR8)));
        assert_eq!(load_through_argument_address(Opcode::LdindI, Value::Float(1.0)), Err(Trap::Unsupported(Opcode::LdindI)));
        assert_eq!(load_through_argument_address(Opcode::LdindR4, Value::Int64(0x3F80_0000)), Err(Trap::Unsupported(Opcode::LdindR4)));
    }

    #[test]
    fn a_narrower_read_of_a_long_or_of_a_wider_native_int_is_refused() {
        assert_eq!(load_through_argument_address(Opcode::LdindI4, Value::Int64(0x0000_0001_0000_0002)),
            Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(load_at_argument_offset(Opcode::LdindI4, 4, Value::Int64(0x0000_0001_0000_0002)),
            Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(load_through_argument_address(Opcode::LdindU1, Value::Int64(0x1FF)), Err(Trap::Unsupported(Opcode::LdindU1)));
        assert_eq!(load_through_static_address(Opcode::LdindU4, Value::Int64(5)), Err(Trap::Unsupported(Opcode::LdindU4)));
        if crate::native_pointer_size() == 8 {
            assert_eq!(load_through_argument_address(Opcode::LdindI4, Value::NativeInt(5)), Err(Trap::Unsupported(Opcode::LdindI4)));
        }
        assert_eq!(super::load_from_slot_at(Value::Int64(0x1_2000_0010), Opcode::LdindI, 4, None, true),
            Err(Trap::Unsupported(Opcode::LdindI)));
        assert_eq!(super::load_from_slot_at(Value::NativeInt(5), Opcode::LdindI4, 4, None, true), Ok(Value::Int32(5)));
        assert_eq!(load_through_argument_address(Opcode::LdindU2, Value::NativeInt(0x1_0002)), Ok(Some(Value::Int32(2))));
        assert_eq!(load_through_static_address(Opcode::LdindI1, Value::Int32(0x1FF)), Ok(Some(Value::Int32(-1))));
        if crate::native_pointer_size() == 8 {
            assert_eq!(load_through_argument_address(Opcode::LdindI8, Value::NativeInt(-2)), Ok(Some(Value::Int64(-2))));
            assert_eq!(load_through_argument_address(Opcode::LdindI, Value::Int64(-2)), Ok(Some(Value::NativeInt(-2))));
        }
    }

    /// Runs `ldarga.s 0; conv.u; ldc.i4 <offset>; add; <load>; ret`: the CIL of a `T*` read `offset` bytes
    /// into the argument through `(byte*)&argument` (ECMA-334 2nd ed 25.4, 25.5.3 and 25.5.6; ECMA-335 6th ed
    /// III.3.1 and III.3.42).
    fn load_at_argument_offset(load: Opcode, offset: i32, argument: Value) -> Result<Option<Value>, Trap> {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::new(Opcode::LdcI4, Operand::Int32(offset)),
            Instruction::simple(Opcode::Add),
            Instruction::simple(load),
            ret(),
        ]), 1);
        super::run(&module, &mut Vm::new(), main, vec![argument])
    }

    #[test]
    fn a_byte_walk_into_a_variable_is_refused() {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::new(Opcode::LdcI4, Operand::Int32(0x7f)),
            Instruction::simple(Opcode::StindI1),
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::LdindU1),
            ret(),
        ]), 1);
        assert_eq!(super::run(&module, &mut Vm::new(), main, vec![Value::Int32(0x0403_0201)]),
            Err(Trap::Unsupported(Opcode::LdindU1)));
        assert_eq!(load_at_argument_offset(Opcode::LdindU1, 0, Value::Int32(0x0403_0201)), Ok(Some(Value::Int32(1))));
        for offset in 1..4 {
            assert_eq!(load_at_argument_offset(Opcode::LdindU1, offset, Value::Int32(0x0403_0201)),
                Err(Trap::Unsupported(Opcode::LdindU1)), "offset {offset}");
        }
        assert_eq!(load_at_argument_offset(Opcode::LdindU1, 7, Value::Int64(0x0800_0000_0000_0000)),
            Err(Trap::Unsupported(Opcode::LdindU1)));
        assert_eq!(load_at_argument_offset(Opcode::LdindU2, 2, Value::Int32(-1)), Err(Trap::Unsupported(Opcode::LdindU2)));
    }

    #[test]
    #[cfg(feature = "float")]
    fn a_floating_variable_is_read_only_at_the_width_its_value_holds() {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::LdcR8, Operand::Float64(1.0)),
            Instruction::simple(Opcode::Stloc0),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::LdindR4),
            ret(),
        ]), 0);
        assert_eq!(super::run(&module, &mut Vm::new(), main, Vec::new()), Err(Trap::Unsupported(Opcode::LdindR4)));
        assert_eq!(load_through_argument_address(Opcode::LdindU1, Value::Single(1.0)),
            Err(Trap::Unsupported(Opcode::LdindU1)));
        assert_eq!(load_through_argument_address(Opcode::LdindI4, Value::Float(1.0)),
            Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(load_at_argument_offset(Opcode::LdindU1, 3, Value::Single(1.0)),
            Err(Trap::Unsupported(Opcode::LdindU1)));
        assert_eq!(load_inside_element(Opcode::LdindU1, vec![Value::Single(1.0)], 4, 3),
            Err(Trap::Unsupported(Opcode::LdindU1)));
    }

    /// Runs `ldarga.s 0; <constant>; <store>; ldarga.s 0; <load>; ret` on an argument holding `variable`:
    /// a floating value stored through the argument's address, then read back through it.
    #[cfg(feature = "float")]
    fn store_then_load_through_argument_address(store: Opcode, stored: Value, load: Opcode, variable: Value) -> Result<Option<Value>, Trap> {
        let constant = match stored {
            Value::Single(f) => Instruction::new(Opcode::LdcR4, Operand::Float32(f)),
            Value::Float(f) => Instruction::new(Opcode::LdcR8, Operand::Float64(f)),
            other => panic!("not a floating constant: {other:?}"),
        };
        let mut module = Module::new();
        let main = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            constant,
            Instruction::simple(store),
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::simple(load),
            ret(),
        ]), 1);
        super::run(&module, &mut Vm::new(), main, vec![variable])
    }

    #[test]
    fn a_floating_store_of_a_value_that_is_not_floating_is_refused() {
        for store in [Opcode::StindR4, Opcode::StindR8] {
            let mut module = Module::new();
            let main = module.add_method_image(0, method(vec![
                Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
                Instruction::new(Opcode::LdcI4, Operand::Int32(0x3F80_0000)),
                Instruction::simple(store),
                Instruction::simple(Opcode::Ldarg0),
                ret(),
            ]), 1);
            let refused = if cfg!(feature = "float") { Trap::TypeMismatch(store) } else { Trap::Unsupported(store) };
            assert_eq!(super::run(&module, &mut Vm::new(), main, vec![Value::Int32(0)]), Err(refused), "{store:?}");
        }
    }

    #[test]
    #[cfg(feature = "float")]
    fn a_floating_store_through_a_pointer_takes_its_instructions_width() {
        assert_eq!(store_then_load_through_argument_address(Opcode::StindR8, Value::Single(1.0), Opcode::LdindI4, Value::Float(0.0)),
            Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(store_then_load_through_argument_address(Opcode::StindR8, Value::Single(1.5), Opcode::LdindR8, Value::Float(0.0)),
            Ok(Some(Value::Float(1.5))));
        assert_eq!(store_then_load_through_argument_address(Opcode::StindR4, Value::Float(1.0), Opcode::LdindI4, Value::Single(0.0)),
            Ok(Some(Value::Int32(0x3F80_0000))));
    }

    /// Runs `code` as a method whose argument and local slots have the declared kinds `args` and
    /// `locals`, called with `arguments`.
    fn run_with_slot_kinds(code: Vec<Instruction>, args: &[SlotKind], locals: &[SlotKind], arguments: Vec<Value>) -> Result<Option<Value>, Trap> {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(code), u16::try_from(args.len()).unwrap());
        module.set_method_slot_kinds(main, args, locals);
        super::run(&module, &mut Vm::new(), main, arguments)
    }

    #[test]
    fn a_whole_value_store_into_recorded_storage_replaces_only_a_value_of_its_own() {
        let int = |n: i32| Instruction::new(Opcode::LdcI4, Operand::Int32(n));
        let through_local = |first: Vec<Instruction>, stored: Vec<Instruction>, store: Opcode| {
            let mut code = first;
            code.extend([Instruction::new(Opcode::StlocS, Operand::Variable(0)), Instruction::new(Opcode::LdlocaS, Operand::Variable(0))]);
            code.extend(stored);
            code.extend([Instruction::simple(store), Instruction::new(Opcode::LdlocS, Operand::Variable(0)), ret()]);
            code
        };
        let unrecorded = |code| run_with_slot_kinds(code, &[], &[SlotKind::Unknown], Vec::new());
        assert_eq!(unrecorded(through_local(vec![int(0x0403_0201)], vec![int(9)], Opcode::StindI4)), Ok(Some(Value::Int32(9))));
        let native = || vec![int(5), Instruction::simple(Opcode::ConvU)];
        assert_eq!(
            run_with_slot_kinds(through_local(vec![Instruction::simple(Opcode::Ldnull)], native(), Opcode::StindI), &[], &[SlotKind::Reference], Vec::new()),
            Err(Trap::Unsupported(Opcode::StindI))
        );
        let null_pointer = || vec![int(0), Instruction::simple(Opcode::ConvU)];
        let into_native = |stored| run_with_slot_kinds(through_local(null_pointer(), stored, Opcode::StindRef), &[], &[SlotKind::Native], Vec::new());
        assert_eq!(into_native(native()), Ok(Some(Value::NativeInt(5))));
        assert_eq!(into_native(vec![Instruction::simple(Opcode::Ldnull)]), Ok(Some(Value::NativeInt(0))));
    }

    #[test]
    fn an_unwritten_local_is_its_declared_kinds_zero() {
        let read = |kind, load| run_with_slot_kinds(vec![
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(load),
            ret(),
        ], &[], &[kind], Vec::new());
        assert_eq!(read(SlotKind::Primitive(PrimKind::I8), Opcode::LdindI8), Ok(Some(Value::Int64(0))));
        #[cfg(feature = "float")]
        assert_eq!(read(SlotKind::Primitive(PrimKind::F8), Opcode::LdindI8), Ok(Some(Value::Int64(0))));
        assert_eq!(read(SlotKind::Native, Opcode::LdindI), Ok(Some(Value::NativeInt(0))));
    }

    #[test]
    #[cfg(feature = "float")]
    fn a_floating_value_stored_in_a_local_or_an_argument_takes_its_declared_width() {
        let f8 = SlotKind::Primitive(PrimKind::F8);
        let f4 = SlotKind::Primitive(PrimKind::F4);
        let store_then_load = |constant: Instruction, kind, load| run_with_slot_kinds(vec![
            constant,
            Instruction::simple(Opcode::Stloc0),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(load),
            ret(),
        ], &[], &[kind], Vec::new());
        assert_eq!(store_then_load(Instruction::new(Opcode::LdcR4, Operand::Float32(1.5)), f8, Opcode::LdindR8),
            Ok(Some(Value::Float(1.5))));
        assert_eq!(store_then_load(Instruction::new(Opcode::LdcR8, Operand::Float64(1.0)), f4, Opcode::LdindR4),
            Ok(Some(Value::Single(1.0))));
        assert_eq!(store_then_load(Instruction::new(Opcode::LdcR4, Operand::Float32(1.0)), f8, Opcode::LdindU4),
            Ok(Some(Value::Int32(0))));
        let argument = |code| run_with_slot_kinds(code, &[f8], &[], vec![Value::Single(1.5)]);
        assert_eq!(argument(vec![
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::simple(Opcode::LdindR8),
            ret(),
        ]), Ok(Some(Value::Float(1.5))));
        assert_eq!(argument(vec![
            Instruction::new(Opcode::LdcR4, Operand::Float32(1.0)),
            Instruction::new(Opcode::StargS, Operand::Variable(0)),
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::simple(Opcode::LdindU4),
            ret(),
        ]), Ok(Some(Value::Int32(0))));
    }

    /// Runs `ldc.i4 <initial>; stloc.0; ldloca.s 0; conv.u; ldc.i4 <displacement>; add; <constant>;
    /// <store>; ldloc.0; ret` on a local of declared kind `kind`: a store through a pointer
    /// `displacement` bytes into the variable, then the variable read whole.
    fn store_inside_local(kind: SlotKind, initial: Instruction, displacement: i32, constant: Instruction, store: Opcode) -> Result<Option<Value>, Trap> {
        run_with_slot_kinds(vec![
            initial,
            Instruction::simple(Opcode::Stloc0),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::new(Opcode::LdcI4, Operand::Int32(displacement)),
            Instruction::simple(Opcode::Add),
            constant,
            Instruction::simple(store),
            Instruction::simple(Opcode::Ldloc0),
            ret(),
        ], &[], &[kind], Vec::new())
    }

    #[test]
    fn a_store_through_a_pointer_replaces_only_the_bytes_it_writes() {
        let i4 = SlotKind::Primitive(PrimKind::I4);
        let int = |n: i32| Instruction::new(Opcode::LdcI4, Operand::Int32(n));
        assert_eq!(store_inside_local(i4, int(0x0403_0201), 0, int(0x7f), Opcode::StindI1), Ok(Some(Value::Int32(0x0403_027f))));
        assert_eq!(store_inside_local(i4, int(0x0403_0201), 1, int(0xAA), Opcode::StindI1), Ok(Some(Value::Int32(0x0403_AA01))));
        assert_eq!(store_inside_local(i4, int(0x0403_0201), 2, int(0x7777), Opcode::StindI2), Ok(Some(Value::Int32(0x7777_0201))));
        #[cfg(feature = "float")]
        assert_eq!(store_inside_local(i4, int(0), 0, Instruction::new(Opcode::LdcR4, Operand::Float32(1.0)), Opcode::StindR4),
            Ok(Some(Value::Int32(0x3F80_0000))));
        assert_eq!(store_inside_local(i4, int(1), 0, Instruction::new(Opcode::LdcI8, Operand::Int64(2)), Opcode::StindI8),
            Err(Trap::Unsupported(Opcode::StindI8)));
        assert_eq!(store_inside_local(i4, int(1), 4, int(2), Opcode::StindI1), Err(Trap::Unsupported(Opcode::StindI1)));
        assert_eq!(store_inside_local(SlotKind::Primitive(PrimKind::I1), int(0), 0, int(0xC8), Opcode::StindI1),
            Ok(Some(Value::Int32(-56))));
        assert_eq!(store_inside_local(i4, int(1), 0, int(-7), Opcode::StindI4), Ok(Some(Value::Int32(-7))));
    }

    /// Runs `<initial>; stloc.0; ldloca.s 0; conv.u; ldc.i4 <displacement>; add; <load>; ret` on a local
    /// of declared kind `kind`: a read through a pointer `displacement` bytes into the variable.
    fn load_inside_local(kind: SlotKind, initial: Instruction, displacement: i32, load: Opcode) -> Result<Option<Value>, Trap> {
        run_with_slot_kinds(vec![
            initial,
            Instruction::simple(Opcode::Stloc0),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::new(Opcode::LdcI4, Operand::Int32(displacement)),
            Instruction::simple(Opcode::Add),
            Instruction::simple(load),
            ret(),
        ], &[], &[kind], Vec::new())
    }

    #[test]
    fn a_read_through_a_pointer_takes_the_variables_declared_bytes() {
        let i4 = SlotKind::Primitive(PrimKind::I4);
        let int = |n: i32| Instruction::new(Opcode::LdcI4, Operand::Int32(n));
        assert_eq!(load_inside_local(i4, int(-1), 1, Opcode::LdindU1), Ok(Some(Value::Int32(255))));
        assert_eq!(load_inside_local(i4, int(0x0403_0201), 2, Opcode::LdindU2), Ok(Some(Value::Int32(0x0403))));
        assert_eq!(load_inside_local(SlotKind::Primitive(PrimKind::I8), Instruction::new(Opcode::LdcI8, Operand::Int64(0x0000_0001_0000_0001)), 4, Opcode::LdindI4),
            Ok(Some(Value::Int32(1))));
        #[cfg(feature = "float")]
        {
            let f8 = SlotKind::Primitive(PrimKind::F8);
            let one = || Instruction::new(Opcode::LdcR8, Operand::Float64(1.0));
            assert_eq!(load_inside_local(f8, one(), 0, Opcode::LdindU4), Ok(Some(Value::Int32(0))));
            assert_eq!(load_inside_local(f8, one(), 4, Opcode::LdindU4), Ok(Some(Value::Int32(0x3FF0_0000))));
            assert_eq!(load_inside_local(f8, Instruction::new(Opcode::LdcR4, Operand::Float32(1.0)), 0, Opcode::LdindU4),
                Ok(Some(Value::Int32(0))));
        }
        assert_eq!(load_inside_local(i4, int(1), 0, Opcode::LdindI8), Err(Trap::Unsupported(Opcode::LdindI8)));
        assert_eq!(load_inside_local(i4, int(1), 4, Opcode::LdindU1), Err(Trap::Unsupported(Opcode::LdindU1)));
    }

    #[test]
    fn a_read_after_a_partial_store_sees_both() {
        assert_eq!(run_with_slot_kinds(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(0x0403_0201)),
            Instruction::simple(Opcode::Stloc0),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::new(Opcode::LdcI4, Operand::Int32(0x7f)),
            Instruction::simple(Opcode::StindI1),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::LdindU1),
            ret(),
        ], &[], &[SlotKind::Primitive(PrimKind::I4)], Vec::new()), Ok(Some(Value::Int32(2))));
    }

    #[test]
    fn a_full_width_store_into_a_pointer_variable_leaves_a_native_int() {
        let native = crate::native_pointer_size();
        let (full, zero) = if native == 8 {
            (Opcode::StindI8, Instruction::new(Opcode::LdcI8, Operand::Int64(0)))
        } else {
            (Opcode::StindI4, Instruction::new(Opcode::LdcI4, Operand::Int32(0)))
        };
        assert_eq!(run_with_slot_kinds(vec![
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            zero,
            Instruction::simple(full),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::StindI1),
            Instruction::simple(Opcode::Ldloc0),
            ret(),
        ], &[], &[SlotKind::Native], Vec::new()), Ok(Some(Value::NativeInt(1))));
        #[cfg(feature = "float")]
        {
            let (floating, zero) = if native == 8 {
                (Opcode::StindR8, Instruction::new(Opcode::LdcR8, Operand::Float64(0.0)))
            } else {
                (Opcode::StindR4, Instruction::new(Opcode::LdcR4, Operand::Float32(0.0)))
            };
            assert_eq!(run_with_slot_kinds(vec![
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                zero,
                Instruction::simple(floating),
                Instruction::simple(Opcode::Ldloc0),
                ret(),
            ], &[], &[SlotKind::Native], Vec::new()), Ok(Some(Value::NativeInt(0))));
        }
        assert_eq!(run_with_slot_kinds(vec![
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::Ldnull),
            Instruction::simple(Opcode::StindI),
            Instruction::simple(Opcode::Ldloc0),
            ret(),
        ], &[], &[SlotKind::Native], Vec::new()), Ok(Some(Value::NativeInt(0))));
    }

    #[test]
    #[cfg(feature = "float")]
    fn a_full_width_store_of_a_value_without_the_instructions_type_is_refused() {
        assert_eq!(run_with_slot_kinds(vec![
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::new(Opcode::LdcR4, Operand::Float32(1.0)),
            Instruction::simple(Opcode::StindI4),
            Instruction::simple(Opcode::Ldloc0),
            ret(),
        ], &[], &[SlotKind::Primitive(PrimKind::I4)], Vec::new()), Err(Trap::TypeMismatch(Opcode::StindI4)));
    }

    #[test]
    fn a_whole_value_stored_through_a_variables_address_takes_its_declared_kind() {
        assert_eq!(run_with_slot_kinds(vec![
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::Ldnull),
            Instruction::simple(Opcode::StindRef),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::LdcI40),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::Ceq),
            ret(),
        ], &[], &[SlotKind::Native], Vec::new()), Ok(Some(Value::Int32(1))));
        assert_eq!(run_with_slot_kinds(vec![
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::new(Opcode::Initobj, Operand::Token(Token::new(0x1b, 9))),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::LdindI),
            ret(),
        ], &[], &[SlotKind::Native], Vec::new()), Ok(Some(Value::NativeInt(0))));
        #[cfg(feature = "float")]
        assert_eq!(run_with_slot_kinds(vec![
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::new(Opcode::LdcR4, Operand::Float32(1.0)),
            Instruction::new(Opcode::Stobj, Operand::Token(Token::new(0x01, 9))),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::LdindU4),
            ret(),
        ], &[], &[SlotKind::Primitive(PrimKind::F8)], Vec::new()), Ok(Some(Value::Int32(0))));
    }

    #[test]
    fn a_null_reference_stored_in_a_pointer_local_is_the_null_pointer() {
        assert_eq!(run_with_slot_kinds(vec![
            Instruction::simple(Opcode::Ldnull),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::LdcI40),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::Ceq),
            ret(),
        ], &[], &[SlotKind::Native], Vec::new()), Ok(Some(Value::Int32(1))));
    }

    /// Constructs with `newobj` a two-dimensional array, `rows` by `columns`, whose element shape the
    /// loader recorded as `element`; returns the module, the machine, the array, and a method of the
    /// module that reads its argument, a pointer, with `read`.
    fn new_rectangular(element: CastElem, rows: i32, columns: i32, read: Opcode) -> (Module, Vm, ObjectRef, MethodId) {
        let ctor = Token::new(0x0a, 1);
        let mut module = Module::new();
        module.mark_md_array_ctor(0, ctor, 2);
        module.bind_cast_elem(0, ctor, CastElem::MdArray { element: Box::new(element), rank: 2 });
        let main = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(rows)),
            Instruction::new(Opcode::LdcI4, Operand::Int32(columns)),
            Instruction::new(Opcode::Newobj, Operand::Token(ctor)),
            ret(),
        ]), 0);
        let reader = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(read),
            ret(),
        ]), 1);
        let mut vm = Vm::new();
        let array = match super::run(&module, &mut vm, main, Vec::new()) {
            Ok(Some(Value::Object(array))) => array,
            other => panic!("newobj of a rectangular array gave {other:?}"),
        };
        (module, vm, array, reader)
    }

    #[test]
    fn a_rectangular_arrays_elements_start_at_their_element_types_zero() {
        let zero = |element| {
            let (_, vm, array, _) = new_rectangular(element, 1, 1, Opcode::LdindI4);
            vm.heap().md_array_get(array, &[0, 0])
        };
        assert_eq!(zero(CastElem::Prim(CastPrim::I8)), Some(Value::Int64(0)));
        assert_eq!(zero(CastElem::Prim(CastPrim::U8)), Some(Value::Int64(0)));
        assert_eq!(zero(CastElem::Prim(CastPrim::U1)), Some(Value::Int32(0)));
        #[cfg(feature = "float")]
        {
            assert_eq!(zero(CastElem::Prim(CastPrim::F8)), Some(Value::Float(0.0)));
            assert_eq!(zero(CastElem::Prim(CastPrim::F4)), Some(Value::Single(0.0)));
        }
        assert_eq!(zero(CastElem::Prim(CastPrim::I)), Some(Value::NativeInt(0)));
        assert_eq!(zero(CastElem::Pointer(Box::new(CastElem::Prim(CastPrim::I4)))), Some(Value::NativeInt(0)));
        assert_eq!(zero(CastElem::String), Some(Value::Null));
        assert_eq!(zero(CastElem::Array(Box::new(CastElem::Prim(CastPrim::I4)))), Some(Value::Null));
    }

    #[test]
    fn a_rectangular_arrays_element_width_is_its_element_types() {
        let (module, mut vm, array, reader) = new_rectangular(CastElem::Prim(CastPrim::I8), 1, 3, Opcode::LdindI8);
        assert!(vm.heap_mut().md_array_set(array, &[0, 1], Value::Int64(11)));
        assert!(vm.heap_mut().md_array_set(array, &[0, 2], Value::Int64(22)));
        let walked = |byte_offset| vec![Value::ByRef(Location::Element { array, index: 0, byte_offset })];
        assert_eq!(super::run(&module, &mut vm, reader, walked(8)), Ok(Some(Value::Int64(11))));
        let (module, mut vm, array, reader) = new_rectangular(CastElem::Prim(CastPrim::U1), 2, 3, Opcode::LdindU1);
        for (flat, value) in (1..=6).enumerate() {
            let indices = [i32::try_from(flat / 3).unwrap(), i32::try_from(flat % 3).unwrap()];
            assert!(vm.heap_mut().md_array_set(array, &indices, Value::Int32(value)));
        }
        let walked = |byte_offset| vec![Value::ByRef(Location::Element { array, index: 0, byte_offset })];
        assert_eq!(super::run(&module, &mut vm, reader, walked(4)), Ok(Some(Value::Int32(5))));
    }

    /// Runs `code` as a method of one argument, called on `vm` with a pointer to `location`.
    fn run_on_pointer(vm: &mut Vm, code: Vec<Instruction>, location: Location) -> Result<Option<Value>, Trap> {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(code), 1);
        super::run(&module, vm, main, vec![Value::ByRef(location)])
    }

    /// `ldarg.0; <load>; ret`: a read through a pointer to `location`.
    fn read_through(vm: &mut Vm, location: Location, load: Opcode) -> Result<Option<Value>, Trap> {
        run_on_pointer(vm, vec![Instruction::simple(Opcode::Ldarg0), Instruction::simple(load), ret()], location)
    }

    /// `ldarg.0; <constant>; <store>; ldnull; ret`: a store through a pointer to `location`.
    fn store_through(vm: &mut Vm, location: Location, constant: Instruction, store: Opcode) -> Result<Option<Value>, Trap> {
        run_on_pointer(vm, vec![
            Instruction::simple(Opcode::Ldarg0),
            constant,
            Instruction::simple(store),
            Instruction::simple(Opcode::Ldnull),
            ret(),
        ], location)
    }

    /// A pointer to element `index` of `array`, moved `byte_offset` bytes on by pointer arithmetic.
    fn element_at(array: ObjectRef, index: usize, byte_offset: u32) -> Location {
        Location::Element { array, index, byte_offset }
    }

    /// A packed array of `kind` holding `values`.
    fn packed(vm: &mut Vm, kind: PrimKind, values: &[Value]) -> ObjectRef {
        let array = vm.heap_mut().alloc_packed_array(kind, values.len(), 0);
        for (index, value) in values.iter().enumerate() {
            assert!(vm.heap_mut().array_set(array, index, value.clone()));
        }
        array
    }

    #[test]
    fn a_read_through_a_pointer_into_a_packed_array_takes_its_bytes() {
        let mut vm = Vm::new();
        let ints = packed(&mut vm, PrimKind::I4, &[Value::Int32(0x0403_0201)]);
        assert_eq!(read_through(&mut vm, element_at(ints, 0, 1), Opcode::LdindU1), Ok(Some(Value::Int32(2))));
        assert_eq!(read_through(&mut vm, element_at(ints, 0, 2), Opcode::LdindU2), Ok(Some(Value::Int32(0x0403))));
        let bytes = packed(&mut vm, PrimKind::U1, &[1, 2, 3, 4].map(Value::Int32));
        assert_eq!(read_through(&mut vm, element_at(bytes, 0, 0), Opcode::LdindI4), Ok(Some(Value::Int32(0x0403_0201))));
        let shorts = packed(&mut vm, PrimKind::I2, &[Value::Int32(-256), Value::Int32(0x1234)]);
        assert_eq!(read_through(&mut vm, element_at(shorts, 0, 1), Opcode::LdindU2), Ok(Some(Value::Int32(0x34FF))));
        let eight = packed(&mut vm, PrimKind::U1, &[1, 2, 3, 4, 5, 6, 7, 8].map(Value::Int32));
        let native = if crate::native_pointer_size() == 8 { 0x0807_0605_0403_0201 } else { 0x0403_0201 };
        assert_eq!(read_through(&mut vm, element_at(eight, 0, 0), Opcode::LdindI), Ok(Some(Value::NativeInt(native))));
        let grid = vm.heap_mut().alloc_md_array_packed(vec![2, 3], PrimKind::U1, 0);
        for (flat, value) in (1..=6).enumerate() {
            assert!(vm.heap_mut().array_set(grid, flat, Value::Int32(value)));
        }
        assert_eq!(read_through(&mut vm, element_at(grid, 0, 0), Opcode::LdindI4), Ok(Some(Value::Int32(0x0403_0201))));
        assert_eq!(read_through(&mut vm, element_at(bytes, 0, 1), Opcode::LdindI4), Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(read_through(&mut vm, element_at(bytes, 3, 1), Opcode::LdindU1), Err(Trap::Unsupported(Opcode::LdindU1)));
    }

    #[test]
    fn a_store_through_a_pointer_into_a_packed_array_replaces_only_its_bytes() {
        let mut vm = Vm::new();
        let int = |n: i32| Instruction::new(Opcode::LdcI4, Operand::Int32(n));
        let bytes = packed(&mut vm, PrimKind::U1, &[0, 0, 0, 0].map(Value::Int32));
        assert_eq!(store_through(&mut vm, element_at(bytes, 0, 0), int(0x0403_0201), Opcode::StindI4), Ok(Some(Value::Null)));
        let all = |vm: &Vm| (0..4).map(|index| vm.heap().array_get(bytes, index)).collect::<Vec<_>>();
        assert_eq!(all(&vm), [1, 2, 3, 4].map(|n| Some(Value::Int32(n))));
        let ints = packed(&mut vm, PrimKind::I4, &[Value::Int32(0x1234_5678)]);
        assert_eq!(store_through(&mut vm, element_at(ints, 0, 1), int(0xAA), Opcode::StindI1), Ok(Some(Value::Null)));
        assert_eq!(vm.heap().array_get(ints, 0), Some(Value::Int32(0x1234_AA78)));
        assert_eq!(store_through(&mut vm, element_at(bytes, 0, 2), int(-1), Opcode::StindI4), Err(Trap::Unsupported(Opcode::StindI4)));
        assert_eq!(all(&vm), [1, 2, 3, 4].map(|n| Some(Value::Int32(n))));
    }

    #[test]
    fn a_pointer_stepped_back_inside_an_array_reaches_the_element_before() {
        let mut vm = Vm::new();
        let ints = packed(&mut vm, PrimKind::I4, &[Value::Int32(42), Value::Int32(99)]);
        let back = element_at(ints, 1, 4u32.wrapping_neg());
        assert_eq!(read_through(&mut vm, back.clone(), Opcode::LdindI4), Ok(Some(Value::Int32(42))));
        assert_eq!(store_through(&mut vm, back, Instruction::new(Opcode::LdcI4, Operand::Int32(77)), Opcode::StindI4),
            Ok(Some(Value::Null)));
        assert_eq!(vm.heap().array_get(ints, 0), Some(Value::Int32(77)));
        let native = crate::native_pointer_size();
        let pointers = vm.heap_mut().alloc_array_with_element_width(
            vec![Value::NativeInt(5), Value::NativeInt(6)], 0, Some(native as usize),
        );
        assert_eq!(read_through(&mut vm, element_at(pointers, 1, native.wrapping_neg()), Opcode::LdindI), Ok(Some(Value::NativeInt(5))));
        assert_eq!(read_through(&mut vm, element_at(ints, 0, 4u32.wrapping_neg()), Opcode::LdindI4), Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(read_through(&mut vm, element_at(pointers, 0, native.wrapping_neg()), Opcode::LdindI), Err(Trap::Unsupported(Opcode::LdindI)));
    }

    #[test]
    fn a_pointer_stepped_back_compares_and_subtracts_at_its_address() {
        let mut vm = Vm::new();
        let ints = packed(&mut vm, PrimKind::I4, &[Value::Int32(42), Value::Int32(99)]);
        let four = || Instruction::new(Opcode::LdcI4, Operand::Int32(4));
        let mut module = Module::new();
        let equal = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            four(),
            Instruction::simple(Opcode::Sub),
            Instruction::simple(Opcode::Ldarg1),
            Instruction::simple(Opcode::Ceq),
            ret(),
        ]), 2);
        let difference = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            four(),
            Instruction::simple(Opcode::Sub),
            Instruction::simple(Opcode::Ldarg1),
            Instruction::simple(Opcode::Sub),
            ret(),
        ]), 2);
        let pointers = vec![Value::ByRef(element_at(ints, 1, 0)), Value::ByRef(element_at(ints, 0, 0))];
        assert_eq!(super::run(&module, &mut vm, equal, pointers.clone()), Ok(Some(Value::Int32(1))));
        assert_eq!(super::run(&module, &mut vm, difference, pointers), Ok(Some(Value::NativeInt(0))));
    }

    /// Allocates with `newarr` an array of `length` elements of an enum whose underlying integer is
    /// `width` bytes wide and signed or not as `unsigned` says.
    fn new_enum_array(vm: &mut Vm, width: u8, unsigned: bool, length: i32) -> ObjectRef {
        let element = Token::new(0x02, 7);
        let mut module = Module::new();
        module.set_enum_constant(0, element.0, 1, String::from("A"));
        module.set_enum_width(0, element.0, width);
        if unsigned {
            module.set_enum_unsigned(0, element.0);
        }
        let main = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(length)),
            Instruction::new(Opcode::Newarr, Operand::Token(element)),
            ret(),
        ]), 0);
        match super::run(&module, vm, main, Vec::new()) {
            Ok(Some(Value::Object(array))) => array,
            other => panic!("newarr of an enum gave {other:?}"),
        }
    }

    #[test]
    fn an_enum_arrays_elements_are_its_underlying_integers_bytes() {
        let mut vm = Vm::new();
        let array = new_enum_array(&mut vm, 4, false, 2);
        assert_eq!(store_through(&mut vm, element_at(array, 0, 0), Instruction::new(Opcode::LdcI8, Operand::Int64(5)), Opcode::StindI),
            Ok(Some(Value::Null)));
        assert!(vm.heap_mut().array_set(array, 1, Value::Int32(2)));
        let native = if crate::native_pointer_size() == 8 { 0x0000_0002_0000_0005 } else { 5 };
        assert_eq!(read_through(&mut vm, element_at(array, 0, 0), Opcode::LdindI), Ok(Some(Value::NativeInt(native))));
        let bytes = new_enum_array(&mut vm, 1, true, 2);
        assert!(vm.heap_mut().array_set(bytes, 1, Value::Int32(7)));
        assert_eq!(read_through(&mut vm, element_at(bytes, 0, 1), Opcode::LdindU1), Ok(Some(Value::Int32(7))));
        let signed = new_enum_array(&mut vm, 1, false, 1);
        assert!(vm.heap_mut().array_set(signed, 0, Value::Int32(-1)));
        assert_eq!(vm.heap().array_get(signed, 0), Some(Value::Int32(-1)));
    }

    #[test]
    fn a_pointer_moved_with_ldind_ref_or_stind_ref_over_a_packed_array_is_a_native_int() {
        let mut vm = Vm::new();
        let ints = packed(&mut vm, PrimKind::I4, &[Value::Int32(1), Value::Int32(2)]);
        assert_eq!(run_on_pointer(&mut vm, vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::new(Opcode::LdcI4, Operand::Int32(5)),
            Instruction::simple(Opcode::ConvI),
            Instruction::simple(Opcode::StindRef),
            Instruction::simple(Opcode::Ldnull),
            ret(),
        ], element_at(ints, 0, 0)), Ok(Some(Value::Null)));
        assert!(vm.heap_mut().array_set(ints, 1, Value::Int32(2)));
        let native = if crate::native_pointer_size() == 8 { 0x0000_0002_0000_0005 } else { 5 };
        assert_eq!(read_through(&mut vm, element_at(ints, 0, 0), Opcode::LdindRef), Ok(Some(Value::NativeInt(native))));
    }

    /// The token of `struct Wrap { int X; }`, of `struct Holder { string S; int X; }`, of `System.Byte`,
    /// of `struct Halves { short A; short B; }`, of `struct Bytes { sbyte A; byte B, C, D; }` and of
    /// `struct Item { byte Small; int Wide; short Tail; }` in the module [`obj_types_module`] builds.
    const WRAP: Token = Token(0x0200_0010);
    const HOLDER: Token = Token(0x0200_0011);
    const BYTE: Token = Token(0x0100_0012);
    const HALVES: Token = Token(0x0200_0013);
    const BYTES: Token = Token(0x0200_0014);
    const ITEM: Token = Token(0x0200_0015);
    /// The token of `class Quad { byte A, B, C, D; }`, of `class Mixed { string S; int X; int Y; }` and of
    /// `class Keeper { Halves H; int Z; }`.
    const QUAD: Token = Token(0x0200_0016);
    const MIXED: Token = Token(0x0200_0017);
    const KEEPER: Token = Token(0x0200_0018);

    /// A module that declares the value types `Wrap`, `Holder`, `System.Byte`, `Halves`, `Bytes` and `Item`
    /// and the classes `Quad`, `Mixed` and `Keeper` under [`WRAP`], [`HOLDER`], [`BYTE`], [`HALVES`],
    /// [`BYTES`], [`ITEM`], [`QUAD`], [`MIXED`] and [`KEEPER`], every one but `System.Byte` with its field
    /// layout (ECMA-335 6th ed II.10.1.2).
    fn obj_types_module() -> Module {
        use crate::module::FieldStorage;
        let mut module = Module::new();
        let pointer = crate::native_pointer_size();
        let mut declare = |token: Token, name: &str, layout: Option<(u32, u32, &[(u32, FieldStorage)])>| {
            let fields = layout.map_or(&[][..], |(_, _, fields)| fields);
            let zero = fields.iter().map(|&(_, storage)| if storage == FieldStorage::Reference { Value::Null } else { Value::Int32(0) });
            let id = module.add_type(zero.collect());
            if let Some((end, alignment, fields)) = layout {
                module.set_type_field_offsets(id, end, alignment, &fields.iter().map(|&(offset, _)| offset).collect::<Vec<_>>());
                module.set_type_field_storage(id, &fields.iter().map(|&(_, storage)| storage).collect::<Vec<_>>());
            }
            module.bind_type_full_name(id, String::from(name));
            module.set_type_is_value_type(id, !matches!(name, "Quad" | "Mixed" | "Keeper"));
            module.bind_type_token(0, token, id);
            id
        };
        let field = |kind| FieldStorage::Primitive(kind);
        declare(WRAP, "Wrap", Some((4, 4, &[(0, field(PrimKind::I4))])));
        declare(HOLDER, "Holder", Some((pointer + 4, pointer, &[(0, FieldStorage::Reference), (pointer, field(PrimKind::I4))])));
        declare(BYTE, "System.Byte", None);
        let halves = declare(HALVES, "Halves", Some((4, 2, &[(0, field(PrimKind::I2)), (2, field(PrimKind::I2))])));
        let bytes = [(0, field(PrimKind::I1)), (1, field(PrimKind::U1)), (2, field(PrimKind::U1)), (3, field(PrimKind::U1))];
        declare(BYTES, "Bytes", Some((4, 1, &bytes)));
        declare(ITEM, "Item", Some((12, 4, &[(0, field(PrimKind::U1)), (4, field(PrimKind::I4)), (8, field(PrimKind::I2))])));
        let quad = [(0, field(PrimKind::U1)), (1, field(PrimKind::U1)), (2, field(PrimKind::U1)), (3, field(PrimKind::U1))];
        declare(QUAD, "Quad", Some((4, 1, &quad)));
        declare(MIXED, "Mixed", Some((pointer + 8, pointer, &[(0, FieldStorage::Reference), (pointer, field(PrimKind::I4)), (pointer + 4, field(PrimKind::I4))])));
        declare(KEEPER, "Keeper", Some((8, 4, &[(0, FieldStorage::ValueType(halves)), (4, field(PrimKind::I4))])));
        module
    }

    /// Runs `code` as [`run_with_slot_kinds`] does, in the module [`obj_types_module`] builds.
    fn run_with_obj_types(code: Vec<Instruction>, args: &[SlotKind], locals: &[SlotKind], arguments: Vec<Value>, vm: &mut Vm) -> Result<Option<Value>, Trap> {
        let mut module = obj_types_module();
        let main = module.add_method_image(0, method(code), u16::try_from(args.len()).unwrap());
        module.set_method_slot_kinds(main, args, locals);
        super::run(&module, vm, main, arguments)
    }

    /// Runs `code` as [`run_with_obj_types`] does, each argument and local a struct variable of the type
    /// its token names, recorded as the loader records a struct variable's type, or, for `None`, an
    /// unmanaged pointer.
    fn run_with_struct_variables(code: Vec<Instruction>, args: &[Option<Token>], locals: &[Option<Token>], arguments: Vec<Value>) -> Result<Option<Value>, Trap> {
        let mut module = obj_types_module();
        let main = module.add_method_image(0, method(code), u16::try_from(args.len()).unwrap());
        let kinds = |slots: &[Option<Token>]| slots.iter().map(|token| if token.is_some() { SlotKind::Struct } else { SlotKind::Native }).collect::<Vec<_>>();
        module.set_method_slot_kinds(main, &kinds(args), &kinds(locals));
        for (argument, slots) in [(true, args), (false, locals)] {
            for (slot, token) in slots.iter().enumerate() {
                if let Some(id) = token.and_then(|token| module.type_id_of(0, token)) {
                    module.set_method_slot_struct_type(main, argument, slot, id);
                }
            }
        }
        super::run(&module, &mut Vm::new(), main, arguments)
    }

    /// A struct of the module [`obj_types_module`] builds, holding the int32 `values` in its fields.
    fn fields(values: &[i32]) -> Value {
        Value::Struct(values.iter().map(|&value| Value::Int32(value)).collect())
    }

    #[test]
    fn a_struct_variable_is_read_and_written_through_its_bytes() {
        let int = |n: i32| Instruction::new(Opcode::LdcI4, Operand::Int32(n));
        let at_argument = |access: Instruction| vec![
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            access,
            ret(),
        ];
        assert_eq!(run_with_struct_variables(at_argument(Instruction::simple(Opcode::LdindU1)), &[Some(WRAP)], &[], vec![fields(&[0x0102_0304])]),
            Ok(Some(Value::Int32(4))));
        assert_eq!(run_with_struct_variables(at_argument(Instruction::new(Opcode::Ldobj, Operand::Token(WRAP))), &[Some(HALVES)], &[], vec![fields(&[1, 2])]),
            Ok(Some(fields(&[0x0002_0001]))));
        let field = |slot| Value::ByRef(Location::Nested { base: Box::new(Location::Arg { frame: 0, slot: 0 }), slot });
        let through_field = |code, values: &[i32], slot| run_with_struct_variables(code, &[Some(BYTES), None], &[], vec![fields(values), field(slot)]);
        let read_int = || vec![Instruction::simple(Opcode::Ldarg1), Instruction::simple(Opcode::LdindI4), ret()];
        assert_eq!(through_field(read_int(), &[1, 2, 3, 4], 0), Ok(Some(Value::Int32(0x0403_0201))));
        assert_eq!(through_field(read_int(), &[-1, 0, 0, 0], 0), Ok(Some(Value::Int32(255))));
        assert_eq!(through_field(vec![
            Instruction::simple(Opcode::Ldarg1),
            Instruction::simple(Opcode::LdcI42),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::LdindU1),
            ret(),
        ], &[1, 2, 3, 4], 0), Ok(Some(Value::Int32(3))));
        assert_eq!(through_field(vec![
            Instruction::simple(Opcode::Ldarg1),
            int(0x0A0B_0C0D),
            Instruction::simple(Opcode::StindI4),
            Instruction::simple(Opcode::Ldarg0),
            ret(),
        ], &[1, 2, 3, 4], 0), Ok(Some(fields(&[0x0D, 0x0C, 0x0B, 0x0A]))));
        assert_eq!(run_with_struct_variables(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::Stloc0),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Add),
            int(0xBBAA),
            Instruction::simple(Opcode::StindI2),
            Instruction::simple(Opcode::Ldloc0),
            ret(),
        ], &[Some(HALVES)], &[Some(HALVES)], vec![fields(&[0x1122, 0x3344])]), Ok(Some(fields(&[i32::from(0xAA22u16 as i16), 0x33BB]))));
        assert_eq!(run_with_struct_variables(vec![
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            int(0x0002_0001),
            Instruction::simple(Opcode::StindI4),
            Instruction::simple(Opcode::Ldloc0),
            ret(),
        ], &[], &[Some(HALVES)], Vec::new()), Ok(Some(fields(&[1, 2]))));
        assert_eq!(run_with_struct_variables(vec![
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::Add),
            Instruction::new(Opcode::Ldobj, Operand::Token(WRAP)),
            ret(),
        ], &[Some(ITEM)], &[], vec![fields(&[17, 1_234_567, 2345])]), Ok(Some(fields(&[1_234_567]))));
        assert_eq!(run_with_struct_variables(vec![
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::Ldarg1),
            Instruction::new(Opcode::Stobj, Operand::Token(WRAP)),
            Instruction::simple(Opcode::Ldarg0),
            ret(),
        ], &[Some(HALVES), Some(WRAP)], &[], vec![fields(&[1, 2]), fields(&[0x0004_0003])]), Ok(Some(fields(&[3, 4]))));
        assert_eq!(run_with_struct_variables(at_argument(Instruction::simple(Opcode::LdindI8)), &[Some(WRAP)], &[], vec![fields(&[1])]),
            Err(Trap::Unsupported(Opcode::LdindI8)));
        let holder = Value::Struct(vec![Value::Null, Value::Int32(7)].into_boxed_slice());
        assert_eq!(run_with_struct_variables(at_argument(Instruction::new(Opcode::Ldobj, Operand::Token(HOLDER))), &[Some(HOLDER)], &[], vec![holder.clone()]),
            Ok(Some(holder.clone())));
        assert_eq!(run_with_struct_variables(read_int(), &[Some(HOLDER), None], &[], vec![holder, field(1)]), Ok(Some(Value::Int32(7))));
    }

    #[test]
    fn a_pointer_to_an_objects_field_reads_and_writes_its_bytes_within_the_value_it_is_in() {
        let types = obj_types_module();
        let id = |token| types.type_id_of(0, token).unwrap();
        let int = |n: i32| Instruction::new(Opcode::LdcI4, Operand::Int32(n));
        let mut vm = Vm::new();
        let quad = vm.heap_mut().alloc_instance(id(QUAD), [1, 2, 3, 4].map(Value::Int32).to_vec());
        let field = |object, slot| Value::ByRef(Location::Field { object, slot });
        let through = |code, argument, vm: &mut Vm| run_with_obj_types(code, &[SlotKind::Native], &[], vec![argument], vm);
        let read = |load| vec![Instruction::simple(Opcode::Ldarg0), Instruction::simple(load), ret()];
        assert_eq!(through(read(Opcode::LdindI4), field(quad, 0), &mut vm), Err(Trap::Unsupported(Opcode::LdindI4)));
        let store = vec![Instruction::simple(Opcode::Ldarg0), int(0x0A0B_0C0D), Instruction::simple(Opcode::StindI4), int(0), ret()];
        assert_eq!(through(store, field(quad, 0), &mut vm), Err(Trap::Unsupported(Opcode::StindI4)));
        assert_eq!((0..4).map(|slot| vm.heap().instance_field(quad, slot)).collect::<Vec<_>>(), [1, 2, 3, 4].map(|n| Some(Value::Int32(n))));
        assert_eq!(through(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::new(Opcode::Ldobj, Operand::Token(WRAP)),
            ret(),
        ], field(quad, 0), &mut vm), Err(Trap::Unsupported(Opcode::Ldobj)));
        let text = vm.heap_mut().alloc_string(&[u16::from(b's')]).unwrap();
        let mixed = vm.heap_mut().alloc_instance(id(MIXED), vec![Value::Object(text), Value::Int32(0x3F80_0000), Value::Int32(6)]);
        #[cfg(feature = "float")]
        assert_eq!(through(read(Opcode::LdindR4), field(mixed, 1), &mut vm), Ok(Some(Value::Single(1.0))));
        assert_eq!(through(read(Opcode::LdindI8), field(mixed, 1), &mut vm), Err(Trap::Unsupported(Opcode::LdindI8)));
        assert_eq!(through(read(Opcode::LdindRef), field(mixed, 0), &mut vm), Ok(Some(Value::Object(text))));
        let halves = fields(&[1, 2]);
        let keeper = vm.heap_mut().alloc_instance(id(KEEPER), vec![halves.clone(), Value::Int32(3)]);
        let h_field = |slot| Value::ByRef(Location::Nested { base: Box::new(Location::Field { object: keeper, slot: 0 }), slot });
        assert_eq!(through(read(Opcode::LdindI4), h_field(0), &mut vm), Ok(Some(Value::Int32(0x0002_0001))));
        assert_eq!(through(read(Opcode::LdindI8), h_field(0), &mut vm), Err(Trap::Unsupported(Opcode::LdindI8)));
        let store_short = vec![Instruction::simple(Opcode::Ldarg0), int(9), Instruction::simple(Opcode::StindI2), int(0), ret()];
        assert_eq!(through(store_short, h_field(1), &mut vm), Ok(Some(Value::Int32(0))));
        assert_eq!(vm.heap().instance_field(keeper, 0), Some(fields(&[1, 9])));
        let boxed = vm.heap_mut().alloc_boxed(crate::module::asm_key(0, HALVES.0), halves);
        let in_box = Value::ByRef(Location::Boxed { object: boxed });
        let b_in_box = Value::ByRef(Location::Nested { base: Box::new(Location::Boxed { object: boxed }), slot: 1 });
        assert_eq!(through(read(Opcode::LdindI4), in_box, &mut vm), Ok(Some(Value::Int32(0x0002_0001))));
        assert_eq!(through(read(Opcode::LdindU1), b_in_box, &mut vm), Ok(Some(Value::Int32(2))));
    }

    /// The tokens of `static int SI` (starting 0x04030201), `static float SF` and `static double SD` (starting
    /// 1.0), and of the field `float F` of `class Floats`, in the module [`statics_module`] builds.
    const SI: Token = Token(0x0400_0010);
    #[cfg(feature = "float")]
    const SF: Token = Token(0x0400_0011);
    #[cfg(feature = "float")]
    const SD: Token = Token(0x0400_0012);
    #[cfg(feature = "float")]
    const FLOATS_F: Token = Token(0x0400_0013);

    /// The module [`obj_types_module`] builds, with the statics [`SI`], [`SF`] and [`SD`] and the class
    /// `Floats` holding the field [`FLOATS_F`], each with its declared storage recorded as the loader
    /// records it.
    fn statics_module() -> Module {
        use crate::module::FieldStorage;
        let mut module = obj_types_module();
        let mut bind = |token: Token, default: Value, kind: PrimKind| {
            module.bind_static_field(0, token, default);
            let slot = module.static_field_slot(0, token).unwrap();
            module.set_static_storage(slot, FieldStorage::Primitive(kind));
        };
        bind(SI, Value::Int32(0x0403_0201), PrimKind::I4);
        #[cfg(feature = "float")]
        {
            bind(SF, Value::Single(0.0), PrimKind::F4);
            bind(SD, Value::Float(1.0), PrimKind::F8);
            let floats = module.add_type(vec![Value::Single(0.0)]);
            module.set_type_field_offsets(floats, 4, 4, &[0]);
            module.set_type_field_storage(floats, &[FieldStorage::Primitive(PrimKind::F4)]);
            module.bind_field(0, FLOATS_F, 0);
            module.bind_field_type(0, FLOATS_F, floats);
        }
        module
    }

    /// Runs `code` with `arguments` in the module [`statics_module`] builds, on a fresh [`Vm`].
    fn run_with_statics(code: Vec<Instruction>, arguments: Vec<Value>) -> Result<Option<Value>, Trap> {
        let mut module = statics_module();
        let main = module.add_method_image(0, method(code), u16::try_from(arguments.len()).unwrap());
        super::run(&module, &mut Vm::new(), main, arguments)
    }

    #[test]
    fn a_pointer_to_a_static_reads_and_writes_its_declared_bytes() {
        let static_address = Instruction::new(Opcode::Ldsflda, Operand::Token(SI));
        assert_eq!(run_with_statics(vec![
            static_address.clone(),
            Instruction::new(Opcode::LdcI4, Operand::Int32(0x7f)),
            Instruction::simple(Opcode::StindI1),
            Instruction::new(Opcode::Ldsfld, Operand::Token(SI)),
            ret(),
        ], Vec::new()), Ok(Some(Value::Int32(0x0403_027F))));
        assert_eq!(run_with_statics(vec![static_address.clone(), Instruction::simple(Opcode::LdindI2), ret()], Vec::new()),
            Ok(Some(Value::Int32(0x0201))));
        assert_eq!(run_with_statics(vec![static_address, Instruction::simple(Opcode::LdindI8), ret()], Vec::new()),
            Err(Trap::Unsupported(Opcode::LdindI8)));
    }

    #[test]
    fn two_field_addresses_on_one_place_order_by_their_recorded_offsets() {
        use crate::module::FieldStorage;
        const S: Token = Token(0x0400_0020);
        const A: Token = Token(0x0400_0021);
        const B: Token = Token(0x0400_0022);
        let ordered = |recorded: bool, first: Token, second: Token, opcode: Opcode| {
            let mut module = obj_types_module();
            let owner = if recorded {
                module.type_id_of(0, HALVES).unwrap()
            } else {
                let unrecorded = module.add_type(vec![Value::Int32(0), Value::Int32(0)]);
                module.set_type_is_value_type(unrecorded, true);
                unrecorded
            };
            module.bind_static_field(0, S, fields(&[0, 0]));
            let slot = module.static_field_slot(0, S).unwrap();
            module.set_static_storage(slot, FieldStorage::ValueType(owner));
            module.bind_field(0, A, 0);
            module.bind_field(0, B, 1);
            let main = module.add_method_image(0, method(vec![
                Instruction::new(Opcode::Ldsflda, Operand::Token(S)),
                Instruction::new(Opcode::Ldflda, Operand::Token(first)),
                Instruction::new(Opcode::Ldsflda, Operand::Token(S)),
                Instruction::new(Opcode::Ldflda, Operand::Token(second)),
                Instruction::simple(opcode),
                ret(),
            ]), 0);
            super::run(&module, &mut Vm::new(), main, Vec::new())
        };
        assert_eq!(ordered(true, A, B, Opcode::Clt), Ok(Some(Value::Int32(1))));
        assert_eq!(ordered(true, B, A, Opcode::Clt), Ok(Some(Value::Int32(0))));
        assert_eq!(ordered(true, B, B, Opcode::Ceq), Ok(Some(Value::Int32(1))));
        assert_eq!(ordered(false, A, B, Opcode::Clt), Err(Trap::Unsupported(Opcode::Clt)));
        assert_eq!(ordered(false, A, B, Opcode::Ceq), Err(Trap::Unsupported(Opcode::Ceq)));
        assert_eq!(ordered(false, B, B, Opcode::Ceq), Ok(Some(Value::Int32(1))));
    }

    #[test]
    #[cfg(feature = "float")]
    fn a_floating_store_into_a_field_or_a_static_takes_its_declared_width() {
        let r8 = |value: f64| Instruction::new(Opcode::LdcR8, Operand::Float64(value));
        let compare = |store: Vec<Instruction>, load: Vec<Instruction>, arguments| {
            let mut code = store;
            code.extend(load);
            code.extend([r8(16_777_217.0), Instruction::simple(Opcode::Ceq), ret()]);
            run_with_statics(code, arguments)
        };
        assert_eq!(compare(
            vec![r8(16_777_217.0), Instruction::new(Opcode::Stsfld, Operand::Token(SF))],
            vec![Instruction::new(Opcode::Ldsfld, Operand::Token(SF))],
            Vec::new(),
        ), Ok(Some(Value::Int32(0))));
        let types = statics_module();
        let floats = types.field_type(0, FLOATS_F).unwrap();
        let mut vm = Vm::new();
        let object = vm.heap_mut().alloc_instance(floats, vec![Value::Single(0.0)]);
        let mut module = statics_module();
        let main = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            r8(16_777_217.0),
            Instruction::new(Opcode::Stfld, Operand::Token(FLOATS_F)),
            Instruction::simple(Opcode::Ldarg0),
            Instruction::new(Opcode::Ldfld, Operand::Token(FLOATS_F)),
            r8(16_777_217.0),
            Instruction::simple(Opcode::Ceq),
            ret(),
        ]), 1);
        assert_eq!(super::run(&module, &mut vm, main, vec![Value::Object(object)]), Ok(Some(Value::Int32(0))));
        assert_eq!(run_with_statics(vec![
            Instruction::new(Opcode::LdcR4, Operand::Float32(1.0)),
            Instruction::new(Opcode::Stsfld, Operand::Token(SD)),
            Instruction::new(Opcode::Ldsflda, Operand::Token(SD)),
            Instruction::simple(Opcode::LdindI4),
            ret(),
        ], Vec::new()), Ok(Some(Value::Int32(0))));
        assert_eq!(run_with_statics(vec![
            Instruction::new(Opcode::Ldsflda, Operand::Token(SD)),
            Instruction::simple(Opcode::ConvU),
            Instruction::new(Opcode::LdcR4, Operand::Float32(0.0)),
            Instruction::simple(Opcode::StindR4),
            Instruction::new(Opcode::Ldsfld, Operand::Token(SD)),
            r8(1.0),
            Instruction::simple(Opcode::Ceq),
            ret(),
        ], Vec::new()), Ok(Some(Value::Int32(1))));
    }

    #[test]
    fn a_builtin_methods_receiver_is_read_through_its_address_as_its_type() {
        fn observe(_vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
            Ok(args.first().cloned())
        }
        let run = |name: &str, stored: i32, address: Vec<Instruction>| {
            let mut module = Module::new();
            let owner = module.add_type(Vec::new());
            module.bind_type_full_name(owner, String::from(name));
            module.set_type_is_value_type(owner, true);
            let token = Token(0x0a00_0001);
            let intrinsic = module.add_intrinsic(0, observe, 0, 1);
            module.set_method_type(intrinsic, owner);
            module.bind_token(0, token, intrinsic);
            let mut code = vec![Instruction::new(Opcode::LdcI4, Operand::Int32(stored)), Instruction::simple(Opcode::Stloc0)];
            code.extend(address);
            code.extend([Instruction::new(Opcode::Call, Operand::Token(token)), ret()]);
            let main = module.add_method_image(0, method(code), 0);
            module.set_method_slot_kinds(main, &[], &[SlotKind::Primitive(PrimKind::I4)]);
            super::run(&module, &mut Vm::new(), main, Vec::new())
        };
        let address = || vec![Instruction::new(Opcode::LdlocaS, Operand::Variable(0)), Instruction::simple(Opcode::ConvU)];
        #[cfg(feature = "float")]
        assert_eq!(run("System.Single", 0x3F80_0000, address()), Ok(Some(Value::Single(1.0))));
        let mut second = address();
        second.extend([Instruction::simple(Opcode::LdcI42), Instruction::simple(Opcode::Add)]);
        assert_eq!(run("System.Char", 0x0042_0041, second), Ok(Some(Value::Int32(0x42))));
        assert_eq!(run("System.Int32", 7, address()), Ok(Some(Value::Int32(7))));
    }

    #[test]
    fn a_pointer_into_a_pinned_string_reads_its_bytes_at_any_byte() {
        let mut vm = Vm::new();
        let mut string = |text: &str| vm.heap_mut().alloc_string(&text.encode_utf16().collect::<Vec<_>>()).unwrap();
        let (one, ab, abc) = (string("\u{0201}"), string("AB"), string("ABC"));
        let at = |string, byte_offset| Value::ByRef(Location::StringChar { string, byte_offset });
        let read = |load, pointer, vm: &mut Vm| {
            run_with_obj_types(vec![Instruction::simple(Opcode::Ldarg0), Instruction::simple(load), ret()], &[SlotKind::Native], &[], vec![pointer], vm)
        };
        assert_eq!(read(Opcode::LdindU1, at(one, 1), &mut vm), Ok(Some(Value::Int32(2))));
        assert_eq!(read(Opcode::LdindI4, at(ab, 0), &mut vm), Ok(Some(Value::Int32(0x0042_0041))));
        assert_eq!(read(Opcode::LdindI2, at(ab, 1), &mut vm), Ok(Some(Value::Int32(0x4200))));
        assert_eq!(read(Opcode::LdindI8, at(abc, 0), &mut vm), Ok(Some(Value::Int64(0x0000_0043_0042_0041))));
        assert_eq!(read(Opcode::LdindI8, at(ab, 0), &mut vm), Err(Trap::Unsupported(Opcode::LdindI8)));
        assert_eq!(read_utf16_through_pointer(&[], &vm, &Location::StringChar { string: ab, byte_offset: 1 }, 0, None), Ok(vec![0x4200]));
        assert_eq!(read_utf16_through_pointer(&[], &vm, &Location::StringChar { string: ab, byte_offset: 0 }, 0, None), Ok(vec![0x41, 0x42]));
    }

    #[test]
    fn a_pointer_to_a_field_of_a_struct_variable_is_the_byte_the_field_starts_at() {
        let field = |slot| Value::ByRef(Location::Nested { base: Box::new(Location::Arg { frame: 0, slot: 0 }), slot });
        let difference = |slot| run_with_struct_variables(vec![
            Instruction::simple(Opcode::Ldarg1),
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::Sub),
            ret(),
        ], &[Some(ITEM), None], &[], vec![fields(&[17, 1_234_567, 2345]), field(slot)]);
        assert_eq!([difference(0), difference(1), difference(2)], [0, 4, 8].map(|offset| Ok(Some(Value::NativeInt(offset)))));
        assert_eq!(run_with_struct_variables(vec![
            Instruction::simple(Opcode::Ldarg1),
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Ceq),
            ret(),
        ], &[Some(BYTES), None], &[], vec![fields(&[1, 2, 3, 4]), field(1)]), Ok(Some(Value::Int32(1))));
    }

    #[test]
    fn ldobj_and_stobj_move_a_value_of_their_type_through_a_pointer_into_a_number() {
        let i4 = SlotKind::Primitive(PrimKind::I4);
        let int = |n: i32| Instruction::new(Opcode::LdcI4, Operand::Int32(n));
        let mut vm = Vm::new();
        assert_eq!(run_with_obj_types(vec![
            int(42),
            Instruction::simple(Opcode::Stloc0),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::new(Opcode::Ldobj, Operand::Token(WRAP)),
            ret(),
        ], &[], &[i4], Vec::new(), &mut vm), Ok(Some(Value::Struct(vec![Value::Int32(42)].into_boxed_slice()))));
        assert_eq!(run_with_obj_types(vec![
            int(0x0403_0201),
            Instruction::simple(Opcode::Stloc0),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Add),
            Instruction::new(Opcode::Ldobj, Operand::Token(BYTE)),
            ret(),
        ], &[], &[i4], Vec::new(), &mut vm), Ok(Some(Value::Int32(2))));
        assert_eq!(run_with_obj_types(vec![
            int(0x0403_0201),
            Instruction::simple(Opcode::Stloc0),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            int(0x7f),
            Instruction::new(Opcode::Stobj, Operand::Token(BYTE)),
            Instruction::simple(Opcode::Ldloc0),
            ret(),
        ], &[], &[i4], Vec::new(), &mut vm), Ok(Some(Value::Int32(0x0403_027f))));
        assert_eq!(run_with_obj_types(vec![
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::Ldarg0),
            Instruction::new(Opcode::Stobj, Operand::Token(WRAP)),
            Instruction::simple(Opcode::Ldloc0),
            ret(),
        ], &[SlotKind::Struct], &[i4], vec![Value::Struct(vec![Value::Int32(7)].into_boxed_slice())], &mut vm), Ok(Some(Value::Int32(7))));
        let bytes = packed(&mut vm, PrimKind::U1, &[1, 2, 3, 4].map(Value::Int32));
        assert_eq!(run_with_obj_types(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::new(Opcode::Ldobj, Operand::Token(WRAP)),
            ret(),
        ], &[SlotKind::ByRef], &[], vec![Value::ByRef(element_at(bytes, 0, 0))], &mut vm),
            Ok(Some(Value::Struct(vec![Value::Int32(0x0403_0201)].into_boxed_slice()))));
        assert_eq!(run_with_obj_types(vec![
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::new(Opcode::Ldobj, Operand::Token(HOLDER)),
            ret(),
        ], &[], &[SlotKind::Primitive(PrimKind::I8)], Vec::new(), &mut vm), Err(Trap::Unsupported(Opcode::Ldobj)));
        let structure = Value::Struct(vec![Value::Int32(9)].into_boxed_slice());
        assert_eq!(run_with_obj_types(vec![
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::new(Opcode::Ldobj, Operand::Token(WRAP)),
            ret(),
        ], &[SlotKind::Struct], &[], vec![structure.clone()], &mut vm), Ok(Some(structure)));
    }

    #[test]
    #[cfg(feature = "float")]
    fn a_floating_element_is_read_and_written_through_its_bytes() {
        let mut vm = Vm::new();
        let doubles = packed(&mut vm, PrimKind::F8, &[Value::Float(0.0)]);
        assert_eq!(store_through(&mut vm, element_at(doubles, 0, 0), Instruction::new(Opcode::LdcR4, Operand::Float32(1.0)), Opcode::StindR4),
            Ok(Some(Value::Null)));
        assert_eq!(read_through(&mut vm, element_at(doubles, 0, 0), Opcode::LdindI8), Ok(Some(Value::Int64(0x3F80_0000))));
        assert_eq!(vm.heap().array_get(doubles, 0), Some(Value::Float(f64::from_bits(0x3F80_0000))));
        let floats = packed(&mut vm, PrimKind::F4, &[Value::Single(1.0)]);
        assert_eq!(read_through(&mut vm, element_at(floats, 0, 3), Opcode::LdindU1), Ok(Some(Value::Int32(0x3F))));
        let floats = packed(&mut vm, PrimKind::F4, &[Value::Single(0.0)]);
        assert_eq!(store_through(&mut vm, element_at(floats, 0, 0), Instruction::new(Opcode::LdcI4, Operand::Int32(0x3F80_0000)), Opcode::StindI4),
            Ok(Some(Value::Null)));
        assert_eq!(vm.heap().array_get(floats, 0), Some(Value::Single(1.0)));
        let grid = vm.heap_mut().alloc_md_array_packed(vec![1, 1], PrimKind::F8, 0);
        assert!(vm.heap_mut().md_array_set(grid, &[0, 0], Value::Single(1.0)));
        assert_eq!(read_through(&mut vm, element_at(grid, 0, 0), Opcode::LdindI4), Ok(Some(Value::Int32(0))));
    }

    #[test]
    fn a_read_wider_than_the_slot_is_refused() {
        assert_eq!(load_through_argument_address(Opcode::LdindI8, Value::Int32(1)),
            Err(Trap::Unsupported(Opcode::LdindI8)));
    }

    #[test]
    #[cfg(feature = "float")]
    fn a_field_or_a_static_assigned_after_a_store_through_its_address_holds_the_value_assigned() {
        let d = Token(0x0400_0001);
        let s = Token(0x0400_0002);
        let mut module = Module::new();
        let class = module.add_type(vec![Value::Float(0.0)]);
        module.bind_field(0, d, 0);
        module.bind_field_type(0, d, class);
        module.bind_static_field(0, s, Value::Float(0.0));
        let assigned = || Instruction::new(Opcode::LdcR8, Operand::Float64(16_777_217.0));
        let zero = || Instruction::new(Opcode::LdcR4, Operand::Float32(0.0));
        let field_program = |module: &mut Module, receiver: Instruction| module.add_method_image(0, method(vec![
            receiver.clone(),
            Instruction::new(Opcode::Ldflda, Operand::Token(d)),
            zero(),
            Instruction::simple(Opcode::StindR4),
            receiver.clone(),
            assigned(),
            Instruction::new(Opcode::Stfld, Operand::Token(d)),
            receiver,
            Instruction::new(Opcode::Ldfld, Operand::Token(d)),
            assigned(),
            Instruction::simple(Opcode::Ceq),
            ret(),
        ]), 1);
        let object_field = field_program(&mut module, Instruction::simple(Opcode::Ldarg0));
        let struct_field = field_program(&mut module, Instruction::new(Opcode::LdargaS, Operand::Variable(0)));
        let static_field = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::Ldsflda, Operand::Token(s)),
            zero(),
            Instruction::simple(Opcode::StindR4),
            assigned(),
            Instruction::new(Opcode::Stsfld, Operand::Token(s)),
            Instruction::new(Opcode::Ldsfld, Operand::Token(s)),
            assigned(),
            Instruction::simple(Opcode::Ceq),
            ret(),
        ]), 0);
        let mut vm = Vm::new();
        let object = Value::Object(vm.heap_mut().alloc_instance(class, vec![Value::Float(0.0)]));
        assert_eq!(super::run(&module, &mut vm, object_field, vec![object]), Ok(Some(Value::Int32(1))));
        let structure = Value::Struct(vec![Value::Float(0.0)].into_boxed_slice());
        assert_eq!(super::run(&module, &mut vm, struct_field, vec![structure]), Ok(Some(Value::Int32(1))));
        assert_eq!(super::run(&module, &mut vm, static_field, Vec::new()), Ok(Some(Value::Int32(1))));
    }

    #[test]
    fn a_byte_walk_into_a_local_is_refused() {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::Stloc0),
            Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::LdcI42),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::LdindU1),
            ret(),
        ]), 1);
        assert_eq!(super::run(&module, &mut Vm::new(), main, vec![Value::Int32(0x0403_0201)]),
            Err(Trap::Unsupported(Opcode::LdindU1)));
    }

    /// Runs `ldarg.0; <load>; ret` on a pointer `byte_offset` bytes past the start of an array of
    /// `width`-byte `elements`.
    fn load_inside_element(load: Opcode, elements: Vec<Value>, width: usize, byte_offset: u32) -> Result<Option<Value>, Trap> {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(load),
            ret(),
        ]), 1);
        let mut vm = Vm::new();
        let array = vm.heap_mut().alloc_array_with_element_width(elements, 0, Some(width));
        super::run(&module, &mut vm, main, vec![Value::ByRef(Location::Element { array, index: 0, byte_offset })])
    }

    /// Runs `ldarg.0; <load>; ret` on a pointer `byte_offset` bytes past the start of a packed array of
    /// `kind` elements holding `bytes`, as `newarr` allocates a primitive array.
    fn load_from_packed(load: Opcode, kind: crate::PrimKind, bytes: Vec<u8>, byte_offset: u32) -> Result<Option<Value>, Trap> {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(load),
            ret(),
        ]), 1);
        let mut vm = Vm::new();
        let array = vm.heap_mut().alloc_packed_from_bytes(kind, bytes, 0);
        super::run(&module, &mut vm, main, vec![Value::ByRef(Location::Element { array, index: 0, byte_offset })])
    }

    #[test]
    fn a_read_wider_than_a_packed_element_reads_the_next_elements_bytes() {
        assert_eq!(load_from_packed(Opcode::LdindI4, crate::PrimKind::U1, vec![1, 2, 3, 4], 0),
            Ok(Some(Value::Int32(0x0403_0201))));
        assert_eq!(load_from_packed(Opcode::LdindI4, crate::PrimKind::U1, vec![42], 0),
            Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(load_from_packed(Opcode::LdindU2, crate::PrimKind::I2, vec![0x00, 0xFF, 0x34, 0x12], 1),
            Ok(Some(Value::Int32(0x34FF))));
        assert_eq!(load_from_packed(Opcode::LdindU1, crate::PrimKind::I2, vec![0x00, 0xFF], 0), Ok(Some(Value::Int32(0))));
        assert_eq!(load_from_packed(Opcode::LdindU4, crate::PrimKind::I4, vec![0, 0, 0x80, 0x3F], 0),
            Ok(Some(Value::Int32(0x3F80_0000))));
    }

    /// Runs `ldarg.0; <load>; ret` on a pointer `byte_offset` bytes into a pinned string of `units`.
    fn load_from_string(load: Opcode, units: &[u16], byte_offset: u32) -> Result<Option<Value>, Trap> {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(load),
            ret(),
        ]), 1);
        let mut vm = Vm::new();
        let string = vm.heap_mut().alloc_string(units).expect("the string allocates");
        super::run(&module, &mut vm, main, vec![Value::ByRef(Location::StringChar { string, byte_offset })])
    }

    #[test]
    fn a_read_inside_or_across_a_pinned_strings_characters_reads_their_bytes() {
        assert_eq!(load_from_string(Opcode::LdindU1, &[0x0201], 1), Ok(Some(Value::Int32(2))));
        assert_eq!(load_from_string(Opcode::LdindI4, &[0x41, 0x42], 0), Ok(Some(Value::Int32(0x0042_0041))));
        assert_eq!(load_from_string(Opcode::LdindU1, &[0x0201], 0), Ok(Some(Value::Int32(1))));
        assert_eq!(load_from_string(Opcode::LdindU2, &[0x41, 0x42], 2), Ok(Some(Value::Int32(0x42))));
        assert_eq!(load_from_string(Opcode::LdindU2, &[0x41, 0x42], 4), Ok(Some(Value::Int32(0))));
    }

    #[test]
    fn a_step_before_or_far_past_an_array_is_refused_without_overflow() {
        assert_eq!(load_from_packed(Opcode::LdindI4, crate::PrimKind::I4, vec![42, 0, 0, 0, 99, 0, 0, 0], 0xFFFF_FFFC),
            Err(Trap::Unsupported(Opcode::LdindI4)));
        let mut module = Module::new();
        let load = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::LdindI4),
            ret(),
        ]), 1);
        let store = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::StindI4),
            Instruction::simple(Opcode::Ldnull),
            ret(),
        ]), 1);
        let mut vm = Vm::new();
        let array = vm.heap_mut().alloc_packed_from_bytes(crate::PrimKind::I4, vec![42, 0, 0, 0], 0);
        let far = Value::ByRef(Location::Element { array, index: usize::MAX, byte_offset: 4 });
        assert_eq!(super::run(&module, &mut vm, load, vec![far.clone()]), Err(Trap::Unsupported(Opcode::LdindI4)));
        assert_eq!(super::run(&module, &mut vm, store, vec![far]), Err(Trap::Unsupported(Opcode::StindI4)));
    }

    #[test]
    fn a_read_inside_a_packed_element_takes_its_byte() {
        let mut module = Module::new();
        let main = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::new(Opcode::LdcI4, Operand::Int32(0xAA)),
            Instruction::simple(Opcode::StindI1),
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::LdindU1),
            ret(),
        ]), 1);
        let mut vm = Vm::new();
        let array = vm.heap_mut().alloc_packed_from_bytes(crate::PrimKind::I4, vec![0x78, 0x56, 0x34, 0x12], 0);
        let pointer = Value::ByRef(Location::Element { array, index: 0, byte_offset: 1 });
        assert_eq!(super::run(&module, &mut vm, main, vec![pointer]), Ok(Some(Value::Int32(0xAA))));
        for offset in 0..8 {
            assert_eq!(load_from_packed(Opcode::LdindU1, crate::PrimKind::I4, vec![1, 2, 3, 4, 5, 6, 7, 8], offset),
                Ok(Some(Value::Int32(offset as i32 + 1))), "offset {offset}");
        }
    }

    #[test]
    fn a_native_int_slot_is_as_wide_as_the_targets_native_int() {
        assert_eq!(super::load_from_slot_at(Value::NativeInt(-1), Opcode::LdindI8, 4, None, true),
            Err(Trap::Unsupported(Opcode::LdindI8)));
        assert_eq!(super::load_from_slot_at(Value::NativeInt(-2), Opcode::LdindI, 4, None, true), Ok(Value::NativeInt(-2)));
        assert_eq!(super::load_from_slot_at(Value::NativeInt(-1), Opcode::LdindI8, 8, None, true), Ok(Value::Int64(-1)));
    }

    #[test]
    fn a_native_int_read_is_bounded_by_the_slot_before_its_value_is_kept() {
        assert_eq!(super::load_from_slot_at(Value::Int32(1), Opcode::LdindI, 4, Some(1), false), Err(Trap::Unsupported(Opcode::LdindI)));
        assert_eq!(super::load_from_slot_at(Value::Int32(0x41), Opcode::LdindI, 4, Some(2), false), Err(Trap::Unsupported(Opcode::LdindI)));
        assert_eq!(super::load_from_slot_at(Value::NativeInt(5), Opcode::LdindI, 8, Some(4), false), Err(Trap::Unsupported(Opcode::LdindI)));
        assert_eq!(super::load_from_slot_at(Value::Int32(-536_813_552), Opcode::LdindI, 4, None, true),
            Err(Trap::Unsupported(Opcode::LdindI)));
        assert_eq!(super::load_from_slot_at(Value::Int32(0x2000_0010), Opcode::LdindI, 4, None, true),
            Ok(Value::NativeInt(0x2000_0010)));
    }

    #[test]
    fn a_native_int_in_raw_memory_is_as_wide_as_the_targets_native_int() {
        assert_eq!(super::indirect_width_at(Opcode::LdindI, 4), Some(4));
        assert_eq!(super::indirect_width_at(Opcode::StindI, 4), Some(4));
        assert_eq!(super::indirect_width_at(Opcode::LdindI, 8), Some(8));
        let raw = [0xFF, 0xFF, 0xFF, 0x7F, 0x34, 0x12, 0, 0];
        assert_eq!(super::stack_loaded_value_at(Opcode::LdindI, raw, 4), Ok(Value::NativeInt(0x7FFF_FFFF)));
        assert_eq!(super::stack_loaded_value_at(Opcode::LdindI, raw, 8), Ok(Value::NativeInt(0x1234_7FFF_FFFF)));
    }

    #[test]
    fn a_native_int_read_on_a_4_byte_target_is_its_own_value_or_refused() {
        let pointer = Value::NativeInt(0xE000_E010);
        assert_eq!(super::load_from_slot_at(pointer.clone(), Opcode::LdindI, 4, None, true), Ok(pointer));
        let high = [0x10, 0xE0, 0x00, 0xE0, 0, 0, 0, 0];
        assert_eq!(super::stack_loaded_value_at(Opcode::LdindI, high, 4), Err(Trap::Unsupported(Opcode::LdindI)));
        let low = [0x10, 0x00, 0x00, 0x20, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(super::stack_loaded_value_at(Opcode::LdindI, low, 4), Ok(Value::NativeInt(0x2000_0010)));
        assert_eq!(super::stack_loaded_value_at(Opcode::LdindI, high, 8), Ok(Value::NativeInt(0xE000_E010)));
    }

    #[test]
    fn a_displacement_before_the_variable_is_refused() {
        assert_eq!(load_at_argument_offset(Opcode::LdindU1, -1, Value::Int32(1)), Err(Trap::Unsupported(Opcode::LdindU1)));
    }

    #[test]
    fn a_byte_walk_past_the_variable_is_refused() {
        assert_eq!(load_at_argument_offset(Opcode::LdindU1, 4, Value::Int32(1)), Err(Trap::Unsupported(Opcode::LdindU1)));
        assert_eq!(load_at_argument_offset(Opcode::LdindI4, 2, Value::Int32(1)), Err(Trap::Unsupported(Opcode::LdindI4)));
    }

    fn constrained_reference_outer_program(nested: bool, local: bool) -> (Module, MethodId, TypeId) {
        let (mut module, _, derived) = constrained_reference_program(false);
        let field = Token(0x0400_0002);
        module.bind_field(0, field, 0);
        let mut body = vec![Instruction::simple(Opcode::Ldarg0)];
        if nested {
            body.push(Instruction::new(Opcode::Ldflda, Operand::Token(field)));
        }
        body.extend([
            Instruction::new(Opcode::Constrained, Operand::Token(Token(0x0200_0001))),
            Instruction::new(Opcode::Callvirt, Operand::Token(Token(0x0600_0001))),
            ret(),
        ]);
        let show = module.add_method_image(0, method(body), 1);
        let show_token = Token(0x0600_0003);
        module.bind_token(0, show_token, show);
        let mut caller = if local {
            vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Stloc0),
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
            ]
        } else {
            vec![Instruction::new(Opcode::LdargaS, Operand::Variable(0))]
        };
        caller.extend([Instruction::new(Opcode::Call, Operand::Token(show_token)), ret()]);
        let main = module.add_method_image(0, method(caller), 1);
        (module, main, derived)
    }

    #[test]
    fn constrained_reference_ref_parameter_reads_the_callers_local() {
        let (module, main, derived) = constrained_reference_outer_program(false, true);
        let mut vm = Vm::new();
        let receiver = vm.heap_mut().alloc_instance(derived, vec![]);
        assert_eq!(super::run(&module, &mut vm, main, vec![Value::Object(receiver)]),
            Ok(Some(Value::Int32(2))));
    }

    #[test]
    fn constrained_reference_in_parameter_reads_the_callers_argument() {
        let (module, main, derived) = constrained_reference_outer_program(false, false);
        let mut vm = Vm::new();
        let receiver = vm.heap_mut().alloc_instance(derived, vec![]);
        assert_eq!(super::run(&module, &mut vm, main, vec![Value::Object(receiver)]),
            Ok(Some(Value::Int32(2))));
    }

    #[test]
    fn constrained_reference_struct_this_field_reads_the_callers_local() {
        let (module, main, derived) = constrained_reference_outer_program(true, true);
        let mut vm = Vm::new();
        let receiver = vm.heap_mut().alloc_instance(derived, vec![]);
        let cell = Value::Struct(vec![Value::Object(receiver)].into_boxed_slice());
        assert_eq!(super::run(&module, &mut vm, main, vec![cell]), Ok(Some(Value::Int32(2))));
    }

    #[test]
    #[cfg(feature = "exceptions")]
    fn constrained_reference_outer_null_throws_before_the_method_runs() {
        let (module, main, _) = constrained_reference_outer_program(false, true);
        let mut vm = Vm::new();
        vm.init_statics(&[Value::Int32(0)]);
        let result = super::run(&module, &mut vm, main, vec![Value::Null]);
        let Err(Trap::UnhandledException(detail)) = result else {
            panic!("expected NullReferenceException, got {result:?}");
        };
        assert_eq!(detail.tag, crate::exception::exception_tag("System.NullReferenceException"));
        assert_eq!(vm.static_field(0), Some(Value::Int32(0)));
    }

    /// [`constrained_reference_program`]'s call made through a constraint token whose cast shape
    /// is lenient, as a class or interface instantiation such as `Box<int>` is, and which resolves
    /// to the same base class.
    fn constrained_instantiation_program() -> (Module, MethodId, TypeId) {
        let (mut module, _, derived) = constrained_reference_program(false);
        let instantiation = Token(0x1B00_0001);
        let base = module.type_id_of(0, Token(0x0200_0001)).unwrap();
        module.bind_type_token(0, instantiation, base);
        module.bind_cast_elem(0, instantiation, CastElem::Lenient);
        let main = module.add_method_image(0, method(vec![
            Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
            Instruction::new(Opcode::Constrained, Operand::Token(instantiation)),
            Instruction::new(Opcode::Callvirt, Operand::Token(Token(0x0600_0001))),
            ret(),
        ]), 1);
        (module, main, derived)
    }

    #[test]
    fn constrained_instantiation_dispatches_on_the_referenced_object() {
        let (module, main, derived) = constrained_instantiation_program();
        let mut vm = Vm::new();
        vm.init_statics(&[Value::Int32(0)]);
        let receiver = vm.heap_mut().alloc_instance(derived, vec![]);
        assert_eq!(super::run(&module, &mut vm, main, vec![Value::Object(receiver)]),
            Ok(Some(Value::Int32(2))));
        assert_eq!(vm.static_field(0), Some(Value::Int32(0)));
    }

    #[test]
    #[cfg(feature = "exceptions")]
    fn constrained_instantiation_null_throws_before_the_method_runs() {
        let (module, main, _) = constrained_instantiation_program();
        let mut vm = Vm::new();
        vm.init_statics(&[Value::Int32(0)]);
        let result = super::run(&module, &mut vm, main, vec![Value::Null]);
        let Err(Trap::UnhandledException(detail)) = result else {
            panic!("expected NullReferenceException, got {result:?}");
        };
        assert_eq!(detail.tag, crate::exception::exception_tag("System.NullReferenceException"));
        assert_eq!(vm.static_field(0), Some(Value::Int32(0)));
    }

    #[test]
    fn static_fields_persist_across_calls() {
        let field = Token(0x0400_0009);
        let bump_token = Token(0x0600_0030);

        let mut module = Module::new();
        module.bind_static_field(0, field, Value::Int32(0));
        let bump = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Ldsfld, Operand::Token(field)),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::Add),
                Instruction::new(Opcode::Stsfld, Operand::Token(field)),
                ret(),
            ]),
            0,
        );
        module.bind_token(0, bump_token, bump);
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Call, Operand::Token(bump_token)),
                Instruction::new(Opcode::Call, Operand::Token(bump_token)),
                Instruction::new(Opcode::Ldsfld, Operand::Token(field)),
                ret(),
            ]),
            0,
        );

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(2)))
        );
    }

    #[cfg(feature = "exceptions")]
    fn cctor_fault_program(access: Opcode, with_finally: bool) -> (Module, MethodId, TypeId) {
        let mut module = Module::new();
        let holder = module.add_type(Vec::new());
        let field = Token(0x0400_0001);
        let trace = Token(0x0400_0002);
        let wrapper = Token(0x0100_0001);
        module.bind_type_full_name(holder, String::from("Broken"));
        module.bind_static_field(0, field, Value::Int32(0));
        module.bind_static_field(0, trace, Value::Int32(0));
        module.bind_static_slot_range(0, 1, holder);
        module.bind_catch_type_tag(0, wrapper,
            crate::exception::exception_tag("System.TypeInitializationException"));
        let cctor = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldnull),
            Instruction::simple(Opcode::Throw),
        ]), 0);
        module.set_method_type(cctor, holder);
        module.add_static_ctor(cctor);

        let mut code = vec![
            Instruction::simple(Opcode::Ldnull),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(if access == Opcode::Stsfld { Opcode::LdcI47 } else { Opcode::Nop }),
            Instruction::new(access, Operand::Token(field)),
            Instruction::simple(if access == Opcode::Stsfld { Opcode::Nop } else { Opcode::Pop }),
            Instruction::new(Opcode::Leave, Operand::Target(0)),
        ];
        let mut handlers = Vec::new();
        if with_finally {
            let start = code.len() as u32;
            code.extend([
                Instruction::simple(Opcode::LdcI41),
                Instruction::new(Opcode::Stsfld, Operand::Token(trace)),
                Instruction::simple(Opcode::Endfinally),
            ]);
            handlers.push(EhClause {
                try_range: InstructionRange { start: 3, end: start },
                handler_range: InstructionRange { start, end: code.len() as u32 },
                kind: EhKind::Finally,
            });
        }
        let handler = code.len() as u32;
        code.extend([
            Instruction::simple(Opcode::Stloc0),
            Instruction::new(Opcode::Leave, Operand::Target(handler + 2)),
            Instruction::simple(Opcode::Ldloc0),
            ret(),
        ]);
        code[5].operand = Operand::Target(handler + 2);
        handlers.push(EhClause {
            try_range: InstructionRange { start: if with_finally { 2 } else { 3 }, end: handler },
            handler_range: InstructionRange { start: handler, end: handler + 2 },
            kind: EhKind::Catch(wrapper),
        });
        let mut body = method(code);
        body.handlers = handlers.into_boxed_slice();
        let main = module.add_method_image(0, body, 0);
        (module, main, holder)
    }

    #[cfg(feature = "exceptions")]
    #[test]
    fn cctor_failure_at_first_try_instruction_is_caught_and_cached() {
        for access in [Opcode::Ldsfld, Opcode::Ldsflda, Opcode::Stsfld] {
            for frozen in [false, true] {
                let (mut module, main, holder) = cctor_fault_program(access, false);
                if frozen { module.freeze(); }
                let mut vm = Vm::new();
                let first = super::run(&module, &mut vm, main, Vec::new());
                let Ok(Some(Value::Object(wrapper))) = first else {
                    panic!("{access:?}, frozen={frozen}: expected a caught wrapper, got {first:?}");
                };
                assert_eq!(vm.cctor_failure(holder), Some(wrapper));
                let inner = vm.exception_inner(wrapper).expect("the original exception is retained");
                assert_ne!(wrapper, inner);
                assert_eq!(super::run(&module, &mut vm, main, Vec::new()), first);
                assert_eq!(vm.exception_inner(wrapper), Some(inner));
                assert_eq!(vm.cctors_failed.len(), 1);
            }
        }
    }

    #[cfg(feature = "exceptions")]
    #[test]
    fn cctor_failure_at_first_try_instruction_runs_its_finally() {
        for access in [Opcode::Ldsfld, Opcode::Ldsflda, Opcode::Stsfld] {
            let (module, main, _) = cctor_fault_program(access, true);
            let mut vm = Vm::new();
            assert!(matches!(super::run(&module, &mut vm, main, Vec::new()),
                Ok(Some(Value::Object(_)))));
            assert_eq!(vm.static_field(1), Some(Value::Int32(1)), "{access:?}");
        }
    }

    /// `static readonly int Answer = 7;` in the shape the interpreter runs it: a `.cctor` that
    /// writes the field and an entry that reads it. 7 rather than 0 is the whole point -- an
    /// uninitialized static reads its ZERO value, so a program that returns 0 has not failed,
    /// it has answered wrongly.
    fn static_readonly_program() -> (Module, MethodId) {
        let field = Token(0x0400_0011);
        let mut module = Module::new();
        let holder = module.add_type(Vec::new());
        module.bind_static_field(0, field, Value::Int32(0));
        module.bind_static_slot_range(0, 1, holder);

        let cctor = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::LdcI4, Operand::Int32(7)),
                Instruction::new(Opcode::Stsfld, Operand::Token(field)),
                ret(),
            ]),
            0,
        );
        module.set_method_type(cctor, holder);
        module.add_static_ctor(cctor);

        let main = module.add_method_image(
            0,
            method(vec![Instruction::new(Opcode::Ldsfld, Operand::Token(field)), ret()]),
            0,
        );
        (module, main)
    }

    /// The same program with a `.cctor` that INCREMENTS rather than assigns, so running it twice
    /// is visible. `static_readonly_program` cannot see that: assigning 7 twice still reads 7.
    fn counting_cctor_program() -> (Module, MethodId) {
        let field = Token(0x0400_0012);
        let mut module = Module::new();
        let holder = module.add_type(Vec::new());
        module.bind_static_field(0, field, Value::Int32(0));
        module.bind_static_slot_range(0, 1, holder);

        let cctor = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Ldsfld, Operand::Token(field)),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::Add),
                Instruction::new(Opcode::Stsfld, Operand::Token(field)),
                ret(),
            ]),
            0,
        );
        module.set_method_type(cctor, holder);
        module.add_static_ctor(cctor);

        let main = module.add_method_image(
            0,
            method(vec![Instruction::new(Opcode::Ldsfld, Operand::Token(field)), ret()]),
            0,
        );
        (module, main)
    }

    /// `boot_baked` and the lazy trigger are two routes to one initialization, and a module must
    /// not take both: II.10.5.3 runs a type's constructor ONCE. A constructor with a side
    /// effect -- a counter, a device register write, a queued allocation -- is where a second
    /// run stops being invisible.
    #[test]
    fn booting_a_loaded_module_runs_each_constructor_exactly_once() {
        let (module, main) = counting_cctor_program();
        let mut vm = Vm::new();
        let main = super::boot_baked(&module, &mut vm, main).expect("the cctors boot");
        assert_eq!(
            super::run(&module, &mut vm, main, Vec::new()),
            Ok(Some(Value::Int32(1))),
            "2 means the eager boot ALSO fired the lazy trigger and the cctor ran twice"
        );
    }

    /// THE CONTROL, and it is what makes the refusal next door legitimate rather than blanket:
    /// on a LOADED module -- the normal host mode -- the lazy trigger of II.10.5.3 still fires
    /// on first static access, so nothing has to be booted eagerly and nothing is refused.
    #[test]
    fn a_loaded_module_initializes_its_statics_lazily_and_is_never_refused() {
        let (module, main) = static_readonly_program();
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(7))),
            "the lazy path is untouched: no eager boot, and the static still holds 7"
        );
    }

    /// A baked image carries the lazy trigger -- each type's `.cctor` and the owner of each static
    /// slot -- so its first static access runs the constructor, exactly once, as on a loaded
    /// module, and `boot_baked` has nothing to run before the entry point.
    #[cfg(feature = "code-in-place")]
    #[test]
    fn a_baked_image_initializes_each_type_at_its_first_access() {
        let (mut module, main) = counting_cctor_program();
        let image = module.write_baked(Some(main)).expect("bake");
        let image: &'static [u8] = Box::leak(image.into_boxed_slice());
        let (baked, entry) = Module::from_baked(image).expect("boot");
        let entry = entry.expect("the image records its entry point");

        assert!(baked.is_baked(), "a module out of from_baked is baked");
        assert!(
            baked.initializes_types_on_first_access(),
            "the image carries a trigger for every type initializer it lists"
        );
        assert_eq!(
            super::run(&baked, &mut Vm::new(), entry, Vec::new()),
            Ok(Some(Value::Int32(1))),
            "the entry's first static access runs the cctor, with no boot: 0 means it never ran"
        );

        let mut vm = Vm::new();
        let entry = super::boot_baked(&baked, &mut vm, entry).expect("nothing to boot");
        assert!(
            !vm.all_cctors_run(&baked),
            "boot_baked ran or marked a constructor ahead of the type's first access"
        );
        assert_eq!(
            super::run(&baked, &mut vm, entry, Vec::new()),
            Ok(Some(Value::Int32(1))),
            "2 means the constructor ran at boot AND at the access"
        );
    }

    /// AN IMAGE MISSING TABLES IS NOT AN IMAGE OF THIS FORMAT. Every table the format defines is
    /// required, so an image whose directory stops before its last tables is refused rather than
    /// read with them empty: the trigger tables here, and every table after them.
    #[cfg(feature = "code-in-place")]
    #[test]
    fn an_image_missing_its_last_tables_is_refused() {
        let (mut module, main) = static_readonly_program();
        let image = module.write_baked(Some(main)).expect("bake");
        let shorter = crate::module::image_without_trailing_tables(&image, 6);
        let image: &'static [u8] = Box::leak(shorter.into_boxed_slice());
        assert_eq!(Module::from_baked(image).err(), Some(crate::module::BakeError::NotAnImage));
    }

    /// A module holding `System.Nullable<T>` over one value type, wired exactly as the loader
    /// wires a real one: the instantiation is a two-field value type (`hasValue`, then `value`)
    /// and `bind_nullable_underlying` records `T`'s handle against its `TypeId`.
    ///
    /// # Why these two rules are tested from hand-built IL and not from a fixture
    ///
    /// `castclass` and `unbox` with a `Nullable<T>` operand are **unreachable from C#**. Roslyn
    /// lowers `o as int?` / `o is int?` to `isinst` and `(int?)o` to `unbox.any`, so no source any
    /// compiler accepts produces either instruction against a nullable -- a fixture population
    /// cannot contain one. III.4.3 and III.4.32 still specify them, an IL producer may emit them,
    /// and an arm nothing exercises is an arm nobody has run.
    ///
    /// The underlying type gets TWO tokens bound to it, because a box's tag is not always the
    /// handle recorded as `T`: a value boxed in one assembly and tested in another carries two
    /// different tokens for one type, and a same-handle test would answer `false` for it.
    #[cfg(feature = "generics")]
    fn nullable_program(code: Vec<Instruction>) -> (Module, MethodId) {
        let int_def = Token(0x0200_0001);
        let nullable_spec = Token(0x1B00_0001);
        let mut module = Module::new();

        let int_type = module.add_type(vec![]);
        module.set_type_is_value_type(int_type, true);
        module.bind_type_token(0, int_def, int_type);
        module.bind_type_token(0, Token(0x0100_0007), int_type);

        let nullable = module.add_type(vec![Value::Int32(0), Value::Int32(0)]);
        module.set_type_is_value_type(nullable, true);
        module.bind_type_token(0, nullable_spec, nullable);
        module.bind_nullable_underlying(nullable, asm_key(0, int_def.0));

        let main = module.add_method_image(0, method(code), 0);
        (module, main)
    }

    /// III.4.32 with a nullable operand type: `unbox` cannot hand back an interior pointer into the
    /// box (a boxed `Nullable<T>` is a boxed **T**, which has no `Nullable<T>` inside it), so the
    /// standard's own note says an implementation "must manufacture a new Nullable<T> on the heap
    /// and compute the address to the newly allocated object". `ldobj` through that pointer reads
    /// the manufactured instance.
    #[test]
    #[cfg(feature = "generics")]
    fn unbox_of_a_nullable_manufactures_an_instance_and_points_at_it() {
        let (module, main) = nullable_program(
            vec![
                Instruction::new(Opcode::LdcI4, Operand::Int32(42)),
                Instruction::new(Opcode::Box, Operand::Token(Token(0x0200_0001))),
                Instruction::new(Opcode::Unbox, Operand::Token(Token(0x1B00_0001))),
                Instruction::new(Opcode::Ldobj, Operand::Token(Token(0x1B00_0001))),
                ret(),
            ],
        );
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Struct(
                vec![Value::Int32(1), Value::Int32(42)].into_boxed_slice()
            ))),
            "a boxed T unboxes to an instance holding it, with HasValue true"
        );
    }

    /// The other half of III.4.32's exception list, and the half that separates a nullable operand
    /// from every other one: NullReferenceException is specified for "obj is null and valuetype is
    /// a NON-nullable value type". A nullable gets an instance whose `HasValue` is false instead --
    /// the type's own field defaults, the same value `initobj` writes.
    #[test]
    #[cfg(feature = "generics")]
    fn unbox_of_a_nullable_from_null_is_an_instance_with_no_value() {
        let (module, main) = nullable_program(
            vec![
                Instruction::simple(Opcode::Ldnull),
                Instruction::new(Opcode::Unbox, Operand::Token(Token(0x1B00_0001))),
                Instruction::new(Opcode::Ldobj, Operand::Token(Token(0x1B00_0001))),
                ret(),
            ],
        );
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Struct(
                vec![Value::Int32(0), Value::Int32(0)].into_boxed_slice()
            ))),
            "null is not the NullReference site here -- it is HasValue false"
        );
    }

    /// III.4.32's InvalidCastException clause, verbatim: thrown when "valuetype is a Nullable<T>
    /// and obj is not a boxed T". The control for the two tests above -- without it they would pass
    /// against an arm that accepted any box at all.
    #[test]
    #[cfg(feature = "generics")]
    fn unbox_of_a_nullable_from_a_box_of_another_type_is_an_invalid_cast() {
        let unrelated = Token(0x0200_0009);
        let mut code = vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(42)),
            Instruction::new(Opcode::Box, Operand::Token(unrelated)),
            Instruction::new(Opcode::Unbox, Operand::Token(Token(0x1B00_0001))),
            Instruction::new(Opcode::Ldobj, Operand::Token(Token(0x1B00_0001))),
        ];
        code.push(ret());
        let (mut module, main) = nullable_program(code);
        let other = module.add_type(vec![]);
        module.set_type_is_value_type(other, true);
        module.bind_type_token(0, unrelated, other);
        let escaped = super::run(&module, &mut Vm::new(), main, Vec::new());
        let Err(Trap::UnhandledException(_detail)) = escaped else {
            panic!("expected an unhandled exception, got {escaped:?}");
        };
    }

    /// III.4.3, verbatim: "When class is the type System.Nullable<T> and the object's class is T,
    /// castclass succeeds." Reachable only from hand-written IL -- Roslyn spells the same test with
    /// `isinst`.
    #[test]
    #[cfg(feature = "generics")]
    fn castclass_to_a_nullable_succeeds_on_a_boxed_underlying_value() {
        let (module, main) = nullable_program(
            vec![
                Instruction::new(Opcode::LdcI4, Operand::Int32(42)),
                Instruction::new(Opcode::Box, Operand::Token(Token(0x0200_0001))),
                Instruction::new(Opcode::Castclass, Operand::Token(Token(0x1B00_0001))),
                Instruction::new(Opcode::UnboxAny, Operand::Token(Token(0x0200_0001))),
                ret(),
            ],
        );
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(42))),
            "the cast succeeds and leaves the boxed T, which unboxes as T"
        );
    }

    /// The cross-assembly form of the same rule, and the reason the handles are compared through
    /// their resolved `TypeId`: the box is tagged with a SECOND token for the underlying type,
    /// which is what a value boxed in the corlib and tested from a program looks like. A
    /// handle-equality test alone answers `false` here.
    #[test]
    #[cfg(feature = "generics")]
    fn a_nullable_cast_matches_a_box_tagged_through_another_token_for_the_same_type() {
        let (module, main) = nullable_program(
            vec![
                Instruction::new(Opcode::LdcI4, Operand::Int32(42)),
                Instruction::new(Opcode::Box, Operand::Token(Token(0x0100_0007))),
                Instruction::new(Opcode::Isinst, Operand::Token(Token(0x1B00_0001))),
                Instruction::new(Opcode::UnboxAny, Operand::Token(Token(0x0100_0007))),
                ret(),
            ],
        );
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(42))),
            "two tokens for one type are one type"
        );
    }

    /// The negative control for both cast rules: a box of some OTHER type is not an instance of
    /// `Nullable<T>`, and `isinst` answers null rather than succeeding.
    #[test]
    #[cfg(feature = "generics")]
    fn isinst_to_a_nullable_rejects_a_box_of_another_type() {
        let unrelated = Token(0x0200_0009);
        let (mut module, main) = nullable_program(
            vec![
                Instruction::new(Opcode::LdcI4, Operand::Int32(42)),
                Instruction::new(Opcode::Box, Operand::Token(unrelated)),
                Instruction::new(Opcode::Isinst, Operand::Token(Token(0x1B00_0001))),
                ret(),
            ],
        );
        let other = module.add_type(vec![]);
        module.set_type_is_value_type(other, true);
        module.bind_type_token(0, unrelated, other);
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Null)),
            "isinst answers null for a box that is not a boxed T"
        );
    }

    #[test]
    #[cfg(feature = "exceptions")]
    fn castclass_to_an_unrelated_type_throws() {
        let (module, main) = cast_program(Opcode::Castclass);
        let escaped = super::run(&module, &mut Vm::new(), main, Vec::new());
        let Err(Trap::UnhandledException(_detail)) = escaped else {
            panic!("expected an unhandled exception, got {escaped:?}");
        };
    }

    #[test]
    fn isinst_of_an_unrelated_type_is_null() {
        let (module, main) = cast_program(Opcode::Isinst);
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Null))
        );
    }

    #[test]
    fn rectangular_arrays_do_not_satisfy_vector_casts() {
        let vector = Token::new(0x1b, 1);
        let mut module = Module::new();
        module.bind_cast_elem(0, vector, CastElem::Array(Box::new(CastElem::Prim(CastPrim::I4))));
        let main = module.add_method_image(0, method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::new(Opcode::Isinst, Operand::Token(vector)),
            Instruction::simple(Opcode::Ret),
        ]), 1);
        let mut vm = Vm::new();
        let plain = vm.heap_mut().alloc_array(vec![Value::Int32(0)]);
        assert_eq!(super::run(&module, &mut vm, main, vec![Value::Object(plain)]),
            Ok(Some(Value::Object(plain))), "a vector still satisfies its array type");
        assert_eq!(super::run(&module, &mut vm, main, vec![Value::Null]), Ok(Some(Value::Null)));
        for dimensions in [vec![2, 3], vec![4]] {
            let rectangular = vm.heap_mut().alloc_md_array(dimensions);
            assert_eq!(super::run(&module, &mut vm, main, vec![Value::Object(rectangular)]),
                Ok(Some(Value::Null)), "a rectangular array is not a vector");
        }
    }

    #[test]
    fn rectangular_array_clones_keep_their_element_type() {
        let matrix = Token::new(0x1b, 1);
        let other_rank = Token::new(0x1b, 2);
        let other_element = Token::new(0x1b, 3);
        let vector_interface = Token::new(0x1b, 4);
        let mut module = Module::new();
        for (token, rank, element) in [
            (matrix, 2, CastPrim::I4), (other_rank, 3, CastPrim::I4), (other_element, 2, CastPrim::I8),
        ] {
            module.bind_cast_elem(0, token, CastElem::MdArray {
                element: Box::new(CastElem::Prim(element)), rank,
            });
        }
        module.bind_cast_elem(0, vector_interface, CastElem::VectorInterface(Box::new(CastElem::Prim(CastPrim::I4))));
        module.freeze();
        let mut vm = Vm::new();
        let original = vm.heap_mut().alloc_md_array_typed(vec![0, 4], asm_key(0, matrix.0));
        let copy = vm.heap_mut().clone_array(original).expect("clone of rectangular array");
        assert_ne!(original, copy);
        for array in [original, copy] {
            let value = Value::Object(array);
            assert!(super::cast_matches(&module, 0, &vm, &value, matrix));
            for target in [other_rank, other_element, vector_interface] {
                assert!(!super::cast_matches(&module, 0, &vm, &value, target));
            }
        }
    }

    #[test]
    fn each_frame_runs_its_own_method_and_resolves_tokens_in_its_own_assembly() {
        let shared = Token::new(0x06, 1);
        let into_other = Token::new(0x06, 2);
        let mut module = Module::new();
        let one = module.add_method_image(
            0,
            method(vec![Instruction::simple(Opcode::LdcI41), Instruction::simple(Opcode::Ret)]),
            0,
        );
        let two = module.add_method_image(
            1,
            method(vec![Instruction::simple(Opcode::LdcI42), Instruction::simple(Opcode::Ret)]),
            0,
        );
        let callee = module.add_method_image(
            1,
            method(vec![
                Instruction::new(Opcode::Call, Operand::Token(shared)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        let caller = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Call, Operand::Token(shared)),
                Instruction::new(Opcode::LdcI4, Operand::Int32(10)),
                Instruction::simple(Opcode::Mul),
                Instruction::new(Opcode::Call, Operand::Token(into_other)),
                Instruction::simple(Opcode::Add),
                Instruction::new(Opcode::Call, Operand::Token(shared)),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        module.bind_token(0, shared, one);
        module.bind_token(1, shared, two);
        module.bind_token(0, into_other, callee);
        let mut vm = Vm::new();
        let result = crate::interp::run(&module, &mut vm, caller, Vec::new()).expect("run");
        assert_eq!(result, Some(Value::Int32(13)));

        #[cfg(feature = "code-in-place")]
        {
            let image = module.write_baked(Some(caller)).expect("bake");
            let (booted, _) = Module::from_baked(Box::leak(image.into_boxed_slice())).expect("boot");
            let mut vm = Vm::new();
            let result = crate::interp::run(&booted, &mut vm, caller, Vec::new()).expect("baked run");
            assert_eq!(result, Some(Value::Int32(13)));
        }
    }

    /// `L: nop; br L`: a loop that allocates nothing, calls nothing and never ends.
    fn spin_loop(module: &mut Module) -> MethodId {
        module.add_method_image(
            0,
            method(vec![
                Instruction::simple(Opcode::Nop),
                Instruction::new(Opcode::Br, Operand::Target(0)),
            ]),
            0,
        )
    }

    #[cfg(feature = "gc")]
    #[test]
    fn a_collection_asked_for_inside_a_loop_that_allocates_nothing_runs_at_the_next_instruction() {
        let mut module = Module::new();
        let spin = spin_loop(&mut module);
        let mut vm = Vm::new();
        let mut session = Session::new(&module, spin, Vec::new()).expect("session");
        for _ in 0..3 {
            vm.heap_mut().alloc_string(&[0x61]).expect("string");
        }
        for _ in 0..1000 {
            session.step(&module, &mut vm).expect("step");
        }
        assert_eq!(vm.heap().object_count(), 3, "nothing in the loop makes a collection due");
        vm.request_collect();
        session.step(&module, &mut vm).expect("step");
        assert_eq!(vm.heap().object_count(), 0, "the next instruction's safepoint collected");
    }

    /// Allocates one string nothing roots.
    #[cfg(feature = "gc")]
    fn make_garbage(vm: &mut Vm, _module: &Module, _args: &[Value]) -> Result<Option<Value>, Trap> {
        vm.heap_mut().alloc_string(&[0x61]).map_err(|_| Trap::OutOfMemory)?;
        Ok(None)
    }

    #[cfg(feature = "gc")]
    #[test]
    fn a_loop_whose_only_allocation_is_inside_a_call_still_collects_at_the_threshold() {
        let mut module = Module::new();
        let garbage = module.add_intrinsic(
            0,
            make_garbage,
            crate::intrinsic_registry::intrinsic_id("test_make_garbage"),
            0,
        );
        module.bind_token(0, Token::new(0x0A, 1), garbage);
        let churn = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Call, Operand::Token(Token::new(0x0A, 1))),
                Instruction::new(Opcode::Br, Operand::Target(0)),
            ]),
            0,
        );
        let mut vm = Vm::new();
        let mut session = Session::new(&module, churn, Vec::new()).expect("session");
        let mut most = 0;
        for _ in 0..2000 {
            session.step(&module, &mut vm).expect("step");
            most = most.max(vm.heap().object_count());
        }
        assert!(most >= 32, "the loop allocated ({most} objects at most)");
        assert!(most <= 100, "the heap stayed at its threshold, not {most} objects");
    }

    #[cfg(feature = "gc")]
    #[test]
    fn a_collection_interval_collects_after_that_many_allocations() {
        let mut module = Module::new();
        let garbage = module.add_intrinsic(
            0,
            make_garbage,
            crate::intrinsic_registry::intrinsic_id("test_make_garbage"),
            0,
        );
        module.bind_token(0, Token::new(0x0A, 1), garbage);
        let churn = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Call, Operand::Token(Token::new(0x0A, 1))),
                Instruction::new(Opcode::Br, Operand::Target(0)),
            ]),
            0,
        );
        for interval in [1, 10] {
            let mut vm = Vm::new();
            vm.set_collection_interval(core::num::NonZeroUsize::new(interval));
            let mut session = Session::new(&module, churn, Vec::new()).expect("session");
            let mut most = 0;
            let mut collections = 0;
            for _ in 0..2000 {
                let before = vm.heap().object_count();
                session.step(&module, &mut vm).expect("step");
                let after = vm.heap().object_count();
                if after < before {
                    collections += 1;
                }
                most = most.max(after);
            }
            assert_eq!(most, interval, "the heap's peak under an interval of {interval}");
            assert_eq!(collections, 1000 / interval, "the collections under an interval of {interval}");
        }
    }

    /// Allocates a string of as many `a`s as its argument says, and hands it back.
    #[cfg(feature = "gc")]
    fn make_text(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
        let Some(&Value::Int32(length)) = args.first() else {
            return Err(Trap::TypeMismatch(Opcode::Call));
        };
        let units = alloc::vec![0x61u16; usize::try_from(length).unwrap_or(0)];
        let text = vm.heap_mut().alloc_string(&units).map_err(|_| Trap::OutOfMemory)?;
        Ok(Some(Value::Object(text)))
    }

    /// Asks for a collection at the next instruction.
    #[cfg(feature = "gc")]
    fn ask_for_collection(vm: &mut Vm, _module: &Module, _args: &[Value]) -> Result<Option<Value>, Trap> {
        vm.request_collect();
        Ok(None)
    }

    /// The length of the string its argument names.
    #[cfg(feature = "gc")]
    fn text_length(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
        let Some(&Value::Object(text)) = args.first() else {
            return Err(Trap::TypeMismatch(Opcode::Call));
        };
        let units = vm.heap().as_string(text).ok_or(Trap::TypeMismatch(Opcode::Call))?;
        Ok(Some(Value::Int32(i32::try_from(units.len()).unwrap_or(i32::MAX))))
    }

    /// **AN INTRINSIC's ARGUMENTS ARE ROOTS WHILE ITS TYPE's STATIC CONSTRUCTOR RUNS.** A first
    /// call to an intrinsic runs its declaring type's pending `.cctor` before the intrinsic, as a
    /// nested run, after the call has taken its arguments off the caller's stack. A collection in
    /// that constructor must mark and relocate them with everything else.
    ///
    /// The argument here is live -- a local holds it too -- so it survives and MOVES: the garbage
    /// below it is reclaimed. An argument left at its old index then names the object that moved
    /// into that index, and the intrinsic answers about the wrong object, in range and without a
    /// trap: 4, the length of the string after it, for the 3 of its own.
    #[cfg(feature = "gc")]
    #[test]
    fn a_collection_in_an_intrinsics_static_constructor_relocates_the_intrinsics_arguments() {
        let mut module = Module::new();
        let owner = module.add_type(Vec::new());
        let intrinsic = |module: &mut Module, func: crate::module::IntrinsicFn, name: &str, arg_count: u16, row: u32| {
            let id = module.add_intrinsic(0, func, crate::intrinsic_registry::intrinsic_id(name), arg_count);
            module.bind_token(0, Token::new(0x0A, row), id);
            id
        };
        intrinsic(&mut module, make_garbage, "test_make_garbage", 0, 1);
        intrinsic(&mut module, make_text, "test_make_text", 1, 2);
        intrinsic(&mut module, ask_for_collection, "test_ask_for_collection", 0, 3);
        let length = intrinsic(&mut module, text_length, "test_text_length", 1, 4);
        module.set_method_type(length, owner);
        let cctor = module.add_method_image(
            0,
            method(vec![Instruction::new(Opcode::Call, Operand::Token(Token::new(0x0A, 3))), ret()]),
            0,
        );
        module.set_method_type(cctor, owner);
        module.add_static_ctor(cctor);
        let call = |row: u32| Instruction::new(Opcode::Call, Operand::Token(Token::new(0x0A, row)));
        let main = module.add_method_image(
            0,
            method(vec![
                call(1),
                Instruction::new(Opcode::LdcI4, Operand::Int32(3)),
                call(2),
                Instruction::new(Opcode::Stloc, Operand::Variable(0)),
                Instruction::new(Opcode::LdcI4, Operand::Int32(4)),
                call(2),
                Instruction::new(Opcode::Stloc, Operand::Variable(1)),
                Instruction::new(Opcode::Ldloc, Operand::Variable(0)),
                call(4),
                ret(),
            ]),
            0,
        );

        let mut vm = Vm::new();
        assert_eq!(super::run(&module, &mut vm, main, Vec::new()), Ok(Some(Value::Int32(3))));
        assert_eq!(vm.heap().object_count(), 2, "the constructor's collection ran, and took the garbage");
    }

    /// Set by the thread [`a_loop_that_allocates_and_calls_nothing_is_preempted_for_another_thread`]
    /// starts, when it runs.
    static OTHER_THREAD_RAN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

    fn mark_other_thread_ran(_vm: &mut Vm, _module: &Module, _args: &[Value]) -> Result<Option<Value>, Trap> {
        OTHER_THREAD_RAN.store(true, core::sync::atomic::Ordering::SeqCst);
        Ok(None)
    }

    /// Starts a green thread running the static method whose id is the argument.
    fn start_static_thread(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
        let Some(Value::Int32(method)) = args.first() else {
            return Err(Trap::StackUnderflow);
        };
        let id = vm.alloc_thread_id();
        vm.request_spawn(id, *method as MethodId, Value::Null, false);
        Ok(None)
    }

    #[test]
    fn a_loop_that_allocates_and_calls_nothing_is_preempted_for_another_thread() {
        OTHER_THREAD_RAN.store(false, core::sync::atomic::Ordering::SeqCst);
        let mut module = Module::new();
        let mark = module.add_intrinsic(
            0,
            mark_other_thread_ran,
            crate::intrinsic_registry::intrinsic_id("test_mark_other_thread_ran"),
            0,
        );
        let start = module.add_intrinsic(
            0,
            start_static_thread,
            crate::intrinsic_registry::intrinsic_id("test_start_static_thread"),
            1,
        );
        module.bind_token(0, Token::new(0x0A, 1), mark);
        module.bind_token(0, Token::new(0x0A, 2), start);
        let other = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Call, Operand::Token(Token::new(0x0A, 1))),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::LdcI4, Operand::Int32(other as i32)),
                Instruction::new(Opcode::Call, Operand::Token(Token::new(0x0A, 2))),
                Instruction::simple(Opcode::Nop),
                Instruction::new(Opcode::Br, Operand::Target(2)),
            ]),
            0,
        );
        let mut vm = Vm::new();
        let mut quanta = 0u32;
        let ran = run_interruptible(&module, &mut vm, main, Vec::new(), &mut |_vm| {
            quanta += 1;
            !OTHER_THREAD_RAN.load(core::sync::atomic::Ordering::SeqCst) && quanta < 16
        })
        .expect("run");
        assert!(matches!(ran, Ran::Interrupted));
        assert!(OTHER_THREAD_RAN.load(core::sync::atomic::Ordering::SeqCst), "the other thread ran");
        assert!(quanta <= 2, "it ran within two quanta of the spin, not {quanta}");
    }

    #[test]
    fn a_loop_that_allocates_nothing_stops_at_its_breakpoint_and_when_its_service_asks() {
        let mut module = Module::new();
        let spin = spin_loop(&mut module);
        let mut vm = Vm::new();
        let mut session = Session::new(&module, spin, Vec::new()).expect("session");
        session.add_breakpoint(spin, 0);
        for _ in 0..3 {
            let stop = session.continue_(&module, &mut vm).expect("continue");
            assert_eq!(stop.reason, StopReason::Breakpoint);
            assert_eq!(stop.location, Some(CodeLocation { method: spin, instruction: 0 }));
        }
        let mut calls = 0u32;
        let ran = run_interruptible(&module, &mut Vm::new(), spin, Vec::new(), &mut |_vm| {
            calls += 1;
            false
        })
        .expect("run");
        assert!(matches!(ran, Ran::Interrupted));
        assert_eq!(calls, 1, "the first quantum's service call stopped it");
    }

    #[test]
    fn isinst_matches_a_boxed_value_type_via_a_cross_assembly_token() {
        let box_token = Token(0x0100_0007);
        let test_token = Token(0x0100_0009);
        let vt_ctor = Token(0x0600_0041);
        let mut module = Module::new();
        let vt = module.add_type(vec![Value::Int32(0)]);
        module.set_type_is_value_type(vt, true);
        module.bind_type_token(0, box_token, vt);
        module.bind_type_token(0, test_token, vt);
        let ctor = module.add_method_image(0, method(vec![ret()]), 1);
        module.set_method_type(ctor, vt);
        module.bind_token(0, vt_ctor, ctor);
        module.mark_value_type_ctor(0, vt_ctor);
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Newobj, Operand::Token(vt_ctor)),
                Instruction::new(Opcode::Box, Operand::Token(box_token)),
                Instruction::new(Opcode::Isinst, Operand::Token(test_token)),
                Instruction::new(Opcode::BrtrueS, Operand::Target(6)),
                Instruction::simple(Opcode::LdcI40),
                ret(),
                Instruction::simple(Opcode::LdcI41),
                ret(),
            ]),
            0,
        );
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(1)))
        );
    }

    fn cast_program(op: Opcode) -> (Module, MethodId) {
        let b_token = Token(0x0200_0002);
        let a_ctor = Token(0x0600_0040);
        let mut module = Module::new();
        let a = module.add_type(vec![]);
        let b = module.add_type(vec![]);
        module.bind_type_token(0, b_token, b);
        let ctor = module.add_method_image(0, method(vec![ret()]), 1);
        module.set_method_type(ctor, a);
        module.bind_token(0, a_ctor, ctor);
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Newobj, Operand::Token(a_ctor)),
                Instruction::new(op, Operand::Token(b_token)),
                ret(),
            ]),
            0,
        );
        (module, main)
    }

    #[test]
    #[cfg(feature = "exceptions")]
    fn an_unhandled_exception_traps() {
        let e_ctor = Token(0x0600_0050);
        let mut module = Module::new();
        let e = module.add_type(vec![]);
        let ctor = module.add_method_image(0, method(vec![ret()]), 1);
        module.set_method_type(ctor, e);
        module.bind_token(0, e_ctor, ctor);
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Newobj, Operand::Token(e_ctor)),
                Instruction::simple(Opcode::Throw),
                ret(),
            ]),
            0,
        );
        let escaped = super::run(&module, &mut Vm::new(), main, Vec::new());
        let Err(Trap::UnhandledException(_detail)) = escaped else {
            panic!("expected an unhandled exception, got {escaped:?}");
        };
    }

    /// An escaping exception REPORTS ITS TYPE once the module knows that type's name.
    ///
    ///
    #[cfg(feature = "exceptions")]
    #[test]
    fn an_escaping_exception_names_its_type_when_the_module_knows_it() {
        let e_ctor = Token(0x0600_0051);
        let mut module = Module::new();
        let e = module.add_type(vec![]);
        module.bind_type_full_name(e, String::from("System.InvalidOperationException"));
        let ctor = module.add_method_image(0, method(vec![ret()]), 1);
        module.set_method_type(ctor, e);
        module.bind_token(0, e_ctor, ctor);
        let main = module.add_method_image(
            0,
            method(vec![
                Instruction::new(Opcode::Newobj, Operand::Token(e_ctor)),
                Instruction::simple(Opcode::Throw),
                ret(),
            ]),
            0,
        );
        let escaped = super::run(&module, &mut Vm::new(), main, Vec::new());
        let Err(Trap::UnhandledException(detail)) = escaped else {
            panic!("expected an unhandled exception, got {escaped:?}");
        };
        assert_eq!(detail.type_name.as_deref(), Some("System.InvalidOperationException"));
        assert_ne!(detail.tag, 0, "the tag is the one fact both tiers carry");
        assert_eq!(
            format!("{}", Trap::UnhandledException(detail)),
            "unhandled exception: System.InvalidOperationException"
        );
    }


    #[cfg(feature = "exceptions")]
    #[test]
    fn type_identity_runtime_exception_agrees_with_typeof_for_an_alias_record() {
        for mode in 0..3 {
            if mode == 2 && !cfg!(feature = "code-in-place") { continue; }
            for asm in [0, 1] {
                let mut module = Module::new();
                let type_id = module.add_type(vec![]);
                let defining = Token(0x0200_0001);
                let alias = Token(0x0100_0007);
                module.bind_type_token(0, defining, type_id);
                module.bind_type_token(asm, alias, type_id);
                module.bind_reflect_type(asm_key(asm, alias.0), crate::module::ReflectType {
                    full_name: String::from("System.DivideByZeroException"),
                    ..crate::module::ReflectType::default()
                });
                if mode == 1 { module.freeze(); }
                #[cfg(feature = "code-in-place")]
                if mode == 2 {
                    let image = module.write_baked(None).expect("bake exception type records");
                    module = Module::from_baked(Box::leak(image.into_boxed_slice()))
                        .expect("read exception type records").0;
                }
                let mut vm = Vm::new();
                let exception = catchable_fault(&Trap::DivideByZero, &module, &mut vm, None)
                    .expect("runtime exception");
                assert_eq!(vm.exception_type_handle(exception), Some(asm_key(asm, alias.0)));
                let runtime = crate::intrinsics::object_get_type(&mut vm, &module, &[Value::Object(exception)]);
                let named = crate::intrinsics::type_from_handle(&mut vm, &module, &[Value::NativeInt(i64::from(defining.0))]);
                assert_eq!(runtime, named, "mode={mode}, asm={asm}");
            }
        }
    }

    #[cfg(feature = "typed-references")]
    mod typed_references {
        use super::*;

        const INT_TOKEN: Token = Token(0x0100_0001);
        const OTHER_TOKEN: Token = Token(0x0100_0002);

        #[test]
        fn refvalue_round_trips_the_value_through_a_typedref() {
            let mut module = Module::new();
            let main = module.add_method_image(
                0,
                method(vec![
                    Instruction::simple(Opcode::LdcI45),
                    Instruction::simple(Opcode::Stloc0),
                    Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                    Instruction::new(Opcode::Mkrefany, Operand::Token(INT_TOKEN)),
                    Instruction::new(Opcode::Refanyval, Operand::Token(INT_TOKEN)),
                    Instruction::simple(Opcode::LdindI4),
                    ret(),
                ]),
                0,
            );
            assert_eq!(
                super::super::run(&module, &mut Vm::new(), main, Vec::new()),
                Ok(Some(Value::Int32(5)))
            );
        }

        #[test]
        fn refvalue_with_a_mismatched_type_throws() {
            let mut module = Module::new();
            let main = module.add_method_image(
                0,
                method(vec![
                    Instruction::simple(Opcode::LdcI45),
                    Instruction::simple(Opcode::Stloc0),
                    Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                    Instruction::new(Opcode::Mkrefany, Operand::Token(INT_TOKEN)),
                    Instruction::new(Opcode::Refanyval, Operand::Token(OTHER_TOKEN)),
                    Instruction::simple(Opcode::LdindI4),
                    ret(),
                ]),
                0,
            );
            let escaped = super::super::run(&module, &mut Vm::new(), main, Vec::new());
            let Err(Trap::UnhandledException(_detail)) = escaped else {
                panic!("expected an unhandled exception, got {escaped:?}");
            };
        }

        #[test]
        fn reftype_yields_the_referent_type_handle() {
            let mut module = Module::new();
            let main = module.add_method_image(
                0,
                method(vec![
                    Instruction::simple(Opcode::LdcI45),
                    Instruction::simple(Opcode::Stloc0),
                    Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                    Instruction::new(Opcode::Mkrefany, Operand::Token(INT_TOKEN)),
                    Instruction::simple(Opcode::Refanytype),
                    Instruction::simple(Opcode::ConvI4),
                    ret(),
                ]),
                0,
            );
            let expected = asm_key(0, INT_TOKEN.0) as i32;
            assert_eq!(
                super::super::run(&module, &mut Vm::new(), main, Vec::new()),
                Ok(Some(Value::Int32(expected)))
            );
        }

        #[test]
        fn mkrefany_on_a_non_pointer_is_a_type_mismatch() {
            let result = run(vec![
                Instruction::simple(Opcode::LdcI45),
                Instruction::new(Opcode::Mkrefany, Operand::Token(INT_TOKEN)),
                ret(),
            ]);
            assert_eq!(result, Err(Trap::TypeMismatch(Opcode::Mkrefany)));
        }
    }

    #[cfg(feature = "varargs")]
    #[test]
    fn arglist_with_no_varargs_pushes_an_empty_handle() {
        let result = run(vec![
            Instruction::simple(Opcode::Arglist),
            Instruction::simple(Opcode::Ret),
        ]);
        assert!(matches!(result, Ok(Some(Value::Object(_)))));
    }

    fn ret() -> Instruction {
        Instruction::simple(Opcode::Ret)
    }

    mod debug_core {
        use super::*;

        fn call_program() -> (Module, MethodId, MethodId) {
            let mut module = Module::new();
            let add = module.add_method_image(
                0,
                method(vec![
                    Instruction::simple(Opcode::Ldarg0),
                    Instruction::simple(Opcode::Ldarg1),
                    Instruction::simple(Opcode::Add),
                    ret(),
                ]),
                2,
            );
            module.bind_token(0, ADD_TOKEN, add);
            let main = module.add_method_image(
                0,
                method(vec![
                    Instruction::simple(Opcode::LdcI42),
                    Instruction::simple(Opcode::LdcI43),
                    Instruction::new(Opcode::Call, Operand::Token(ADD_TOKEN)),
                    ret(),
                ]),
                0,
            );
            (module, main, add)
        }

        #[test]
        fn continue_stops_at_an_enabled_breakpoint_then_runs_to_completion() {
            let (module, main, _add) = call_program();
            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.add_breakpoint(main, 2);

            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Breakpoint);
            assert_eq!(
                stop.location,
                Some(CodeLocation {
                    method: main,
                    instruction: 2,
                })
            );
            assert_eq!(
                session.frame(0).unwrap().stack,
                [Value::Int32(2), Value::Int32(3)].as_slice()
            );
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.location, None);
            assert_eq!(stop.returned, Some(Value::Int32(5)));
        }

        #[test]
        fn a_disabled_breakpoint_does_not_pause_and_re_enabling_restores_it() {
            let (module, main, _add) = call_program();
            let mut vm = Vm::new();

            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.add_breakpoint(main, 2);
            session.set_breakpoint_enabled(main, 2, false);
            assert!(!session.is_breakpoint_enabled(main, 2));
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.returned, Some(Value::Int32(5)));
            assert_eq!(stop.location, None);

            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.set_breakpoint_enabled(main, 2, false);
            session.set_breakpoint_enabled(main, 2, true);
            assert!(session.is_breakpoint_enabled(main, 2));
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Breakpoint);
            assert_eq!(stop.location.unwrap().instruction, 2);
        }

        #[test]
        fn step_into_descends_into_a_call_while_step_over_runs_it_to_completion() {
            let (module, main, add) = call_program();
            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            assert_eq!(session.depth(), 1);
            let stop = session.step_into(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Step);
            assert_eq!(session.depth(), 2);
            assert_eq!(stop.location.unwrap().method, add);
            assert_eq!(stop.location.unwrap().instruction, 0);

            let (module, main, _add) = call_program();
            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            let stop = session.step_over(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Step);
            assert_eq!(session.depth(), 1);
            assert_eq!(stop.location.unwrap().method, main);
        }

        #[test]
        fn step_out_returns_to_the_caller() {
            let (module, main, _add) = call_program();
            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            assert_eq!(session.depth(), 2);
            let stop = session.step_out(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Step);
            assert_eq!(session.depth(), 1);
            assert_eq!(stop.location.unwrap().method, main);
        }

        #[test]
        fn a_breakpoint_inside_a_stepped_over_call_takes_priority() {
            let (module, main, add) = call_program();
            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.add_breakpoint(add, 2);
            session.step_into(&module, &mut vm).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            let stop = session.step_over(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Breakpoint);
            assert_eq!(stop.location.unwrap().method, add);
            assert_eq!(session.depth(), 2);
        }

        #[test]
        fn location_and_this_report_the_innermost_frame() {
            let v_field = Token(0x0400_00A1);
            let read_token = Token(0x0600_00A1);
            let ctor_token = Token(0x0600_00A2);
            let mut module = Module::new();
            let c = module.add_type(vec![Value::Int32(0)]);
            module.bind_field(0, v_field, 0);
            let ctor = module.add_method_image(0, method(vec![ret()]), 1);
            module.set_method_type(ctor, c);
            module.bind_token(0, ctor_token, ctor);
            let read = module.add_method_image(
                0,
                method(vec![
                    Instruction::simple(Opcode::Ldarg0),
                    Instruction::new(Opcode::Ldfld, Operand::Token(v_field)),
                    ret(),
                ]),
                1,
            );
            module.set_method_type(read, c);
            module.bind_token(0, read_token, read);
            module.set_method_debug(read, "C.Read".into(), alloc::vec!["this".into()]);
            let main = module.add_method_image(
                0,
                method(vec![
                    Instruction::new(Opcode::Newobj, Operand::Token(ctor_token)),
                    Instruction::new(Opcode::Callvirt, Operand::Token(read_token)),
                    ret(),
                ]),
                0,
            );

            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.add_breakpoint(read, 0);
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Breakpoint);
            assert_eq!(session.location().unwrap().method, read);
            assert!(matches!(session.this(&module), Some(Value::Object(_))));
            assert!(session.this(&module).is_some());
            assert_eq!(module.arg_name(main, 0), None);
        }

        #[test]
        fn expand_reads_array_elements_and_object_fields() {
            let mut vm = Vm::new();
            let array = vm
                .heap_mut()
                .alloc_array(alloc::vec![Value::Int32(10), Value::Int32(20), Value::Int32(30)]);
            let instance = vm
                .heap_mut()
                .alloc_instance(0, alloc::vec![Value::Int32(7), Value::Null]);

            let mut module = Module::new();
            let main = module.add_method_image(0, method(vec![ret()]), 0);
            let session = Session::new(&module, main, Vec::new()).unwrap();

            assert!(session.expand(&vm, &Value::Int32(5)).is_empty());
            assert!(!session.is_expandable(&vm, &Value::Int32(5)));

            assert!(session.is_expandable(&vm, &Value::Object(array)));
            let elements = session.expand(&vm, &Value::Object(array));
            assert_eq!(elements.len(), 3);
            assert_eq!(elements[0].name, "[0]");
            assert_eq!(elements[0].value, Value::Int32(10));
            assert_eq!(elements[2].value, Value::Int32(30));

            assert!(session.is_expandable(&vm, &Value::Object(instance)));
            let fields = session.expand(&vm, &Value::Object(instance));
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "field0");
            assert_eq!(fields[0].value, Value::Int32(7));
            assert_eq!(fields[1].value, Value::Null);
        }

        #[cfg(feature = "exceptions")]
        #[test]
        fn pause_on_unhandled_exception_reports_the_exception_instead_of_trapping() {
            let e_ctor = Token(0x0600_00B1);
            let mut module = Module::new();
            let e = module.add_type(vec![]);
            let ctor = module.add_method_image(0, method(vec![ret()]), 1);
            module.set_method_type(ctor, e);
            module.bind_token(0, e_ctor, ctor);
            let main = module.add_method_image(
                0,
                method(vec![
                    Instruction::new(Opcode::Newobj, Operand::Token(e_ctor)),
                    Instruction::simple(Opcode::Throw),
                    ret(),
                ]),
                0,
            );

            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            let escaped = session.continue_(&module, &mut vm);
            let Err(Trap::UnhandledException(_detail)) = escaped else {
                panic!("expected an unhandled exception, got {escaped:?}");
            };

            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.set_pause_on_unhandled_exception(true);
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Exception);
            assert!(session.stopped_exception().is_some());
        }

        #[cfg(feature = "exceptions")]
        #[test]
        fn a_caught_exception_does_not_trigger_the_exception_pause() {
            use lamella_cil::{EhClause, EhKind, InstructionRange};
            let e_ctor = Token(0x0600_00C1);
            let catch_type = Token(0x0100_00C2);
            let mut module = Module::new();
            let e = module.add_type(vec![]);
            let ctor = module.add_method_image(0, method(vec![ret()]), 1);
            module.set_method_type(ctor, e);
            module.bind_token(0, e_ctor, ctor);
            let mut body = method(vec![
                Instruction::new(Opcode::Newobj, Operand::Token(e_ctor)),
                Instruction::simple(Opcode::Throw),
                Instruction::simple(Opcode::Pop),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(42)),
                ret(),
            ]);
            body.handlers = alloc::vec![EhClause {
                try_range: InstructionRange { start: 0, end: 2 },
                handler_range: InstructionRange { start: 2, end: 5 },
                kind: EhKind::Catch(catch_type),
            }]
            .into_boxed_slice();
            let main = module.add_method_image(0, body, 0);

            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.set_pause_on_unhandled_exception(true);
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.returned, Some(Value::Int32(42)));
            assert_eq!(session.stopped_exception(), None);
        }

        #[cfg(feature = "exceptions")]
        #[test]
        fn a_frame_is_set_up_so_that_a_catch_never_allocates() {
            use lamella_cil::{EhClause, EhKind, InstructionRange};
            let catch_type = Token(0x0100_00C2);
            let mut module = Module::new();
            let mut body = method(vec![
                Instruction::simple(Opcode::Nop),
                Instruction::simple(Opcode::Nop),
                Instruction::simple(Opcode::Pop),
                Instruction::simple(Opcode::Pop),
                ret(),
            ]);
            body.handlers = alloc::vec![
                EhClause {
                    try_range: InstructionRange { start: 0, end: 1 },
                    handler_range: InstructionRange { start: 2, end: 3 },
                    kind: EhKind::Catch(catch_type),
                },
                EhClause {
                    try_range: InstructionRange { start: 1, end: 2 },
                    handler_range: InstructionRange { start: 3, end: 4 },
                    kind: EhKind::Catch(catch_type),
                },
            ]
            .into_boxed_slice();
            let main = module.add_method_image(0, body, 0);

            let mut vm = Vm::new();
            let exception = vm.heap_mut().alloc_instance(EXTERNAL_TYPE_ID, Vec::new());
            let frame = new_frame(&mut vm, &module, main, Vec::new()).unwrap();
            let caught = (frame.caught_exceptions.capacity(), frame.caught_exceptions.as_ptr());
            let stack = (frame.stack.capacity(), frame.stack.as_ptr());
            assert!(caught.0 >= 2, "a caught-exception entry for each of the two handlers");

            let mut frames = alloc::vec![frame];
            for handler in [2, 3] {
                complete_finally(
                    &mut frames,
                    &module,
                    &mut vm,
                    AfterFinally::Catch { handler, exception },
                )
                .unwrap();
            }
            let frame = &frames[0];
            assert_eq!(frame.caught_exceptions.len(), 2, "both catches were recorded");
            assert_eq!(
                (frame.caught_exceptions.capacity(), frame.caught_exceptions.as_ptr()),
                caught,
                "recording a catch must not grow the caught-exception list"
            );
            assert_eq!(
                (frame.stack.capacity(), frame.stack.as_ptr()),
                stack,
                "pushing the caught exception must not grow the evaluation stack"
            );
        }

        #[cfg(feature = "exceptions")]
        #[test]
        fn a_rethrow_after_a_nested_catch_re_raises_its_own_handlers_exception() {
            use lamella_cil::{EhClause, EhKind, InstructionRange};
            let e_ctor = Token(0x0600_00C1);
            let catch_type = Token(0x0100_00C2);
            let mut module = Module::new();
            let e = module.add_type(vec![]);
            let ctor = module.add_method_image(0, method(vec![ret()]), 1);
            module.set_method_type(ctor, e);
            module.bind_token(0, e_ctor, ctor);
            let mut body = method(vec![
                Instruction::new(Opcode::Newobj, Operand::Token(e_ctor)),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::Throw),
                Instruction::simple(Opcode::Pop),
                Instruction::new(Opcode::Newobj, Operand::Token(e_ctor)),
                Instruction::simple(Opcode::Throw),
                Instruction::simple(Opcode::Pop),
                Instruction::new(Opcode::Leave, Operand::Target(9)),
                Instruction::simple(Opcode::Rethrow),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::Ceq),
                ret(),
            ]);
            body.handlers = alloc::vec![
                EhClause {
                    try_range: InstructionRange { start: 5, end: 7 },
                    handler_range: InstructionRange { start: 7, end: 9 },
                    kind: EhKind::Catch(catch_type),
                },
                EhClause {
                    try_range: InstructionRange { start: 0, end: 4 },
                    handler_range: InstructionRange { start: 4, end: 10 },
                    kind: EhKind::Catch(catch_type),
                },
                EhClause {
                    try_range: InstructionRange { start: 0, end: 10 },
                    handler_range: InstructionRange { start: 10, end: 13 },
                    kind: EhKind::Catch(catch_type),
                },
            ]
            .into_boxed_slice();
            let main = module.add_method_image(0, body, 0);

            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(
                stop.returned,
                Some(Value::Int32(1)),
                "the rethrow must re-raise the exception its own handler caught, not the nested one"
            );
        }
    }

    /// `Thread.IsAlive` reads TWO premises out of [`Vm::alloc_thread_id`], and until this test
    /// neither was written down anywhere but in that function's four lines.
    ///
    /// An id is never 0, so the managed side's `_id == 0` means "constructed, never started" rather
    /// than "the first thread the scheduler handed out" -- `Join()` has leaned on that since it was
    /// written, and `IsAlive` now answers the unstarted case from it. And an id is never REUSED, so
    /// a `finished_threads` entry cannot be inherited by a later thread and report it dead on
    /// arrival. A thread table that recycled slots would break both silently and in opposite
    /// directions, which is the case for asserting them rather than commenting them.
    #[test]
    fn thread_ids_are_never_zero_and_never_reused() {
        let mut vm = Vm::default();
        let mut seen = BTreeSet::new();
        for _ in 0..1_000 {
            let id = vm.alloc_thread_id();
            assert_ne!(id, 0, "0 is the managed `never started` sentinel, not an id");
            assert!(seen.insert(id), "id {id} was handed out twice");
        }
    }

    /// The finished bit is KEYED and is NOT consumed on read -- both halves of what
    /// [`Vm::is_thread_finished`] promises, and the second is where it parts company with the
    /// timed-park verdict one method above it. [`Vm::take_wait_timed_out`] consumes deliberately, so
    /// a later park cannot read a stale verdict; `IsAlive` may be asked any number of times and must
    /// answer the same thing, so a copy of that consuming shape would have made the SECOND read of a
    /// finished thread say it was alive again.
    #[test]
    fn a_finished_thread_stays_finished_and_the_bit_is_keyed_by_id() {
        let mut vm = Vm::default();
        let finished = vm.alloc_thread_id();
        let other = vm.alloc_thread_id();

        assert!(!vm.is_thread_finished(finished), "an unstarted id is not finished");
        vm.mark_thread_finished(finished);

        for read in 1..=3 {
            assert!(vm.is_thread_finished(finished), "read {read} lost the finished bit");
        }
        assert!(!vm.is_thread_finished(other), "the bit is keyed by id, not a global flag");
    }
}
