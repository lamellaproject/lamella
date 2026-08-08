//! This runtime's [`ReactorEnv`]: the clock and network seam the shared block point waits on.

use lamella_reactor::ReactorEnv;

use crate::object::ObjectModel;
use alloc::vec::Vec;

/// Nanoseconds per millisecond -- the runtime's clock seam is in nanoseconds and the reactor's is in
/// milliseconds, so the conversion lives in one place rather than at each call.
const NANOS_PER_MILLI: i64 = 1_000_000;

/// The seam this runtime blocks through, holding the installed clock rather than borrowing the
/// model.
///
/// Detached from [`ObjectModel`] on purpose: a block point happens when nothing is runnable, and
/// holding a borrow of the whole model across it would mean nothing else could touch the heap while
/// the OS thread is parked. Copying two function pointers costs nothing and keeps the wait
/// independent of the object graph.
#[derive(Clone, Copy, Default)]
pub struct PyReactorEnv {
    /// The monotonic clock an embedder installed, in nanoseconds; `None` when none was.
    monotonic: Option<fn() -> i64>,
    /// The sleep an embedder installed, in nanoseconds; `None` when none was.
    sleep: Option<fn(i64)>,
}

impl PyReactorEnv {
    /// The seam over `clock`/`sleep` as installed, both optional.
    #[must_use]
    pub fn new(monotonic: Option<fn() -> i64>, sleep: Option<fn(i64)>) -> Self {
        Self { monotonic, sleep }
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

impl ReactorEnv for PyReactorEnv {
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

    fn net_poll(&mut self, _timeout_ms: Option<u64>) -> Vec<u32> {
        Vec::new()
    }

    fn net_deregister(&mut self, _socket: u32) {
    }
}

impl ObjectModel {
    /// The reactor seam for this model's installed clock -- what a scheduler hands
    /// `lamella_reactor::block_point` when it has nothing runnable.
    ///
    /// Returns a value rather than a borrow so the wait does not pin the heap: see [`PyReactorEnv`].
    #[must_use]
    pub fn reactor_env(&self) -> PyReactorEnv {
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

    /// Drops `id`'s park -- its timer was cancelled, or it became runnable another way.
    pub fn unpark_waiter(&mut self, id: u32) {
        self.reactor_store().unpark(id);
    }

    /// **The one place this runtime blocks.** With nothing runnable, wait on the nearest parked
    /// deadline and return the waiters to wake (removed from the store). `None` = nothing external to
    /// wait for, which for a loop with work still pending is a deadlock rather than a spin.
    ///
    /// The env is built fresh and detached here on purpose: the wait must not hold a borrow of the
    /// model, or nothing could touch the heap while the OS thread is parked.
    #[must_use]
    pub fn reactor_block_point(&mut self) -> Option<Vec<u32>> {
        let mut env = self.reactor_env();
        self.reactor_store().block_point(&mut env)
    }

    /// The monotonic clock in MILLISECONDS -- the unit deadlines are compared in, so a loop computing
    /// `now + delay` and the reactor comparing against it cannot disagree about the scale.
    #[must_use]
    pub fn reactor_now_millis(&self) -> u64 {
        use lamella_reactor::ReactorEnv;
        self.reactor_env().now_millis().unwrap_or(0)
    }
}
