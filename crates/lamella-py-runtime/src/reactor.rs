//! This runtime's [`ReactorEnv`]: the clock and network seam the shared block point waits on.

use lamella_net_core::NetBackend;
use lamella_reactor::ReactorEnv;

use crate::object::ObjectModel;
use alloc::vec::Vec;

/// Nanoseconds per millisecond -- the runtime's clock seam is in nanoseconds and the reactor's is in
/// milliseconds, so the conversion lives in one place rather than at each call.
const NANOS_PER_MILLI: i64 = 1_000_000;

/// The seam this runtime blocks through: the installed clock, plus the network backend borrowed for
/// the length of one wait.
///
/// **The clock half is COPIED and the network half is BORROWED, and the asymmetry is forced rather
/// than chosen.** A clock is two function pointers, so copying them keeps the wait independent of
/// the object graph. A backend owns the socket table its handles index into, so there is nothing to
/// copy -- it has to be the one the model holds, or a `recv` and the poll that wakes it would be
/// talking about two different sockets.
///
/// What that costs is the lifetime, and what it buys back is that the model is split rather than
/// borrowed whole: [`ObjectModel::reactor_block_point`] hands over the park store and the backend as
/// disjoint fields, so the rest of the model is still free while the OS thread is parked.
#[derive(Default)]
pub struct PyReactorEnv<'a> {
    /// The monotonic clock an embedder installed, in nanoseconds; `None` when none was.
    monotonic: Option<fn() -> i64>,
    /// The sleep an embedder installed, in nanoseconds; `None` when none was.
    sleep: Option<fn(i64)>,
    /// The installed network seam, borrowed for the duration of one wait; `None` when none was.
    net: Option<&'a mut (dyn NetBackend + 'static)>,
}

impl<'a> PyReactorEnv<'a> {
    /// The seam over `clock`/`sleep` as installed, both optional, with NO network backend.
    ///
    /// The shape a caller wants when it is testing the timer half or driving a host that has no
    /// network at all. [`ObjectModel::reactor_block_point`] builds the net-carrying form instead.
    #[must_use]
    pub fn new(monotonic: Option<fn() -> i64>, sleep: Option<fn(i64)>) -> Self {
        Self { monotonic, sleep, net: None }
    }

    /// The same seam with `net` borrowed in, so a park set may hold sockets as well as timers.
    #[must_use]
    pub fn with_net(
        monotonic: Option<fn() -> i64>,
        sleep: Option<fn(i64)>,
        net: Option<&'a mut (dyn NetBackend + 'static)>,
    ) -> Self {
        Self { monotonic, sleep, net }
    }
}

/// The shared park store, wrapped for one reason: `lamella_reactor::Reactor` does not derive `Debug`
/// and [`ObjectModel`] does, so embedding it directly would stop the model deriving its own.
///
/// A wrapper here rather than an edit to the crate below -- the store's behavior is entirely the
/// reactor's and this adds nothing to it. If `Reactor` ever derives `Debug`, this type is deletable
/// and the field can hold it directly.
#[derive(Default)]
pub(crate) struct ParkStore(pub(crate) lamella_reactor::Reactor);

impl core::fmt::Debug for ParkStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ParkStore(..)")
    }
}

impl ReactorEnv for PyReactorEnv<'_> {
    fn now_millis(&self) -> Option<u64> {
        let now = self.monotonic?();
        u64::try_from(now / NANOS_PER_MILLI).ok()
    }

    fn sleep_millis(&mut self, millis: u64) {
        if let Some(sleep) = self.sleep {
            let nanos = i64::try_from(millis).unwrap_or(i64::MAX).saturating_mul(NANOS_PER_MILLI);
            sleep(nanos);
        }
    }

    fn net_poll(&mut self, timeout_ms: Option<u64>) -> Vec<u32> {
        match self.net.as_deref_mut() {
            Some(backend) => backend.poll(timeout_ms),
            None => Vec::new(),
        }
    }

    fn net_deregister(&mut self, socket: u32) {
        if let Some(backend) = self.net.as_deref_mut() {
            backend.deregister(socket);
        }
    }
}

impl ObjectModel {
    /// The reactor seam for this model's installed clock -- what a scheduler hands
    /// `lamella_reactor::block_point` when it has nothing runnable.
    ///
    /// **The CLOCK half only.** A backend cannot be reached through this without borrowing the
    /// model, which is why [`Self::reactor_block_point`] builds its own env by splitting the fields
    /// instead of calling here -- and why this one says so in its name rather than looking complete.
    #[must_use]
    pub fn reactor_clock_env(&self) -> PyReactorEnv<'static> {
        PyReactorEnv::new(self.monotonic_fn(), self.sleep_fn())
    }

    /// Parks waiter `id` until the monotonic-millisecond `deadline`, replacing any prior park for it.
    ///
    /// The id is the event loop's own numbering and is never interpreted here or below -- see the
    /// `reactor` field. What is parked is a TIMER; this runtime has no sockets, so `WaitReason::Io`
    /// has no caller and would have nothing to poll if it did.
    pub fn park_waiter(&mut self, id: u32, deadline: u64) {
        self.reactor_store().park(id, lamella_reactor::WaitReason::Sleep(deadline));
    }

    /// Parks waiter `id` until `socket` is ready, replacing any prior park for it.
    ///
    /// The counterpart to [`Self::park_waiter`] for the other thing there is to wait for. A socket
    /// park carries no deadline: the reactor blocks on the poll set until the backend reports the
    /// handle ready, and a caller that wants a timeout parks a timer for the same id instead.
    pub fn park_waiter_on_socket(&mut self, id: u32, socket: u32) {
        self.reactor_store().park(id, lamella_reactor::WaitReason::Io(socket));
    }

    /// Drops `id`'s park -- its timer was cancelled, or it became runnable another way.
    pub fn unpark_waiter(&mut self, id: u32) {
        self.reactor_store().unpark(id);
    }

    /// **The one place this runtime blocks.** With nothing runnable, wait on the nearest parked
    /// deadline and return the waiters to wake (removed from the store). `None` = nothing external to
    /// wait for, which for a loop with work still pending is a deadlock rather than a spin.
    ///
    /// The env is built by SPLITTING this model's fields rather than by borrowing the whole of it:
    /// the park store and the network backend are disjoint pieces, and the reactor needs both at
    /// once. That is the same disjoint-borrow shape [`ObjectModel::collect`] uses, and it is the
    /// reason the backend can be reached from inside the one wait without a second seam.
    #[must_use]
    pub fn reactor_block_point(&mut self) -> Option<Vec<u32>> {
        let monotonic = self.monotonic_fn();
        let sleep = self.sleep_fn();
        let (store, net) = self.reactor_store_and_net();
        let mut env = PyReactorEnv::with_net(monotonic, sleep, net);
        store.block_point(&mut env)
    }

    /// The monotonic clock in MILLISECONDS -- the unit deadlines are compared in, so a loop computing
    /// `now + delay` and the reactor comparing against it cannot disagree about the scale.
    ///
    /// **Zero when no clock is installed, and that is the reactor's rule rather than a fabricated
    /// time.** With no clock every deadline is already due, so a loop can only ORDER its timers, not
    /// delay them -- and an origin of zero makes a deadline equal to the delay that asked for it,
    /// which is exactly the ordering key. `time.monotonic()` still refuses to answer on such a host,
    /// because a program reading a clock is asking a question this cannot answer; a program that
    /// merely sleeps is not.
    #[must_use]
    pub fn reactor_now_millis(&self) -> u64 {
        use lamella_reactor::ReactorEnv;
        self.reactor_clock_env().now_millis().unwrap_or(0)
    }
}
