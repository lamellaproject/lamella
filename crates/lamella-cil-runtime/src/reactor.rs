//! The scheduler's reactor: the single OS-thread block point, extracted Session-free so the
//! interpreter's green-thread scheduler and the AOT native scheduler drive ONE implementation
//! (`design-aot-reactor-gc-scheduler.md` sec 1). A parked thread waits on a timer deadline or a
//! socket; when nothing is runnable, [`block_point`] performs the ONE blocking wait -- a net poll
//! that honors the nearest timer, or a tickless sleep -- and returns the threads to wake.

use alloc::vec::Vec;

/// Why a thread is parked. The reactor acts on `Sleep`/`Io` (timer + socket waits); a `Join` or
/// `Monitor`-lock park is woken by thread-completion / lock-handoff, NOT here -- such a thread is
/// simply absent from the park set the reactor sees. Its presence WITHOUT any `Sleep`/`Io` park is
/// the deadlock [`block_point`] reports by returning `None`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaitReason {
    /// Sleeping until this monotonic-millisecond deadline (`Thread.Sleep`, or a `Monitor.Wait` timeout).
    Sleep(u64),
    /// Parked until this socket handle is ready (a socket op returned WouldBlock).
    Io(u32),
}

/// The clock + network seam the reactor blocks on. The interpreter's `Vm` implements it; the AOT
/// runtime-support provides the same over its C-ABI net backend, so both schedulers share the
/// block-point algorithm below.
pub trait ReactorEnv {
    /// The monotonic millisecond clock, or `None` if none is installed (a timer is then treated as
    /// already due -- correct wake ORDERING by deadline, without a real delay).
    fn now_millis(&self) -> Option<u64>;
    /// Sleep the OS thread `millis` -- the tickless-idle wait when only timers are pending.
    fn sleep_millis(&mut self, millis: u64);
    /// Block until >= 1 registered socket is ready for its interest OR `timeout_ms` elapses,
    /// returning the ready handles (empty without a backend). The single OS block point when I/O
    /// is pending; the poll HONORS `timeout_ms`, so a timer and sockets are waited on in one call.
    fn net_poll(&mut self, timeout_ms: Option<u64>) -> Vec<u32>;
    /// The allocation-free form of [`net_poll`](ReactorEnv::net_poll): the ready handles are written
    /// into `ready` (up to its length) and the count returned. The default delegates to `net_poll`
    /// and copies -- correct for an alloc-capable env (the interpreter). A `no_std`/no-alloc env (the
    /// AOT staticlib scheduler) OVERRIDES this to poll straight into the caller buffer, and can then
    /// leave `net_poll` a `Vec::new()` stub, since [`block_point_into`] only ever calls this form.
    fn net_poll_into(&mut self, timeout_ms: Option<u64>, ready: &mut [u32]) -> usize {
        let handles = self.net_poll(timeout_ms);
        let n = handles.len().min(ready.len());
        ready[..n].copy_from_slice(&handles[..n]);
        n
    }
    /// Drop `socket` from the poll-set once its waiter is woken (a stale registration must not
    /// produce a spurious later wake; a subsequent socket op re-arms it via `register`).
    fn net_deregister(&mut self, socket: u32);
}

/// The single block point: with no thread runnable, block the OS thread ONCE on the nearest timer
/// deadline and/or the socket poll-set, then return the ids of the threads to wake (sleepers whose
/// deadline passed, io-waiters whose socket readied). `None` = nothing external to wait for -- the
/// remaining threads are lock/join-deadlocked and the scheduler should stop.
///
/// `parks` is the parked threads as `(thread_id, reason)`; a thread parked on a lock/join is simply
/// not included. The returned ids are a subset of `parks`' ids.
///
/// A thread may appear TWICE in `parks` -- an `Io` park bounded by a `Sleep` deadline (the
/// interpreter's timed receive, `Socket.ReceiveTimeout`): the deadline then shapes the poll
/// timeout, the thread wakes when EITHER entry fires, and it may appear in the woken ids once per
/// fired entry -- callers treat the woken ids as a SET. On a deadline (non-ready) wake the socket
/// stays registered; the CALLER deregisters it when it applies the wake.
#[must_use]
pub fn block_point(parks: &[(u32, WaitReason)], env: &mut dyn ReactorEnv) -> Option<Vec<u32>> {
    let mut woken = alloc::vec![0u32; parks.len()];
    let count = block_point_into(parks, env, &mut woken)?;
    woken.truncate(count);
    Some(woken)
}

/// The allocation-free core of [`block_point`]: the ids to wake are written into `woken` (up to its
/// length) and the count returned; `None` = deadlock (nothing external to wait for). For the AOT
/// staticlib scheduler, whose `no_std`/no-alloc target has no allocator for a return `Vec` -- the
/// caller passes a fixed stack buffer (sized to its thread count) and this touches no heap.
///
/// This is the CANONICAL reference for the algorithm. The AOT native scheduler keeps a byte-faithful
/// no-alloc twin (`sched_block_point`) rather than depend on this crate (its device image must not
/// pull the interpreter crate in); a change here must be mirrored there, and the two agree by the
/// shared protocol plus this module's unit tests and the AOT scheduler's on-target e2e.
///
/// A block point drains at most [`READY_BATCH`] ready sockets per call; any beyond it stay
/// registered and re-ready on the next block point, so no wake is lost -- the loop makes progress.
#[must_use]
pub fn block_point_into(
    parks: &[(u32, WaitReason)],
    env: &mut dyn ReactorEnv,
    woken: &mut [u32],
) -> Option<usize> {
    let nearest_deadline = parks
        .iter()
        .filter_map(|(_, reason)| match reason {
            WaitReason::Sleep(deadline) => Some(*deadline),
            WaitReason::Io(_) => None,
        })
        .min();
    let any_io = parks.iter().any(|(_, reason)| matches!(reason, WaitReason::Io(_)));
    if nearest_deadline.is_none() && !any_io {
        return None;
    }
    let timeout = match (nearest_deadline, env.now_millis()) {
        (Some(deadline), Some(now)) => Some(deadline.saturating_sub(now)),
        (Some(_), None) => Some(0),
        (None, _) => None,
    };
    let mut ready_buf = [0u32; READY_BATCH];
    let ready = if any_io {
        let n = env.net_poll_into(timeout, &mut ready_buf);
        &ready_buf[..n]
    } else {
        if let Some(ms) = timeout {
            env.sleep_millis(ms);
        }
        &ready_buf[..0]
    };
    let now = env.now_millis().unwrap_or(u64::MAX);
    let mut count = 0;
    for (id, reason) in parks {
        let wake = match reason {
            WaitReason::Sleep(deadline) => *deadline <= now,
            WaitReason::Io(socket) => {
                let hit = ready.contains(socket);
                if hit {
                    env.net_deregister(*socket);
                }
                hit
            }
        };
        if wake {
            if let Some(slot) = woken.get_mut(count) {
                *slot = *id;
                count += 1;
            }
        }
    }
    Some(count)
}

/// The most ready sockets [`block_point_into`] drains in one call (its fixed stack poll buffer).
/// Beyond this, sockets stay registered and re-ready next block point, so nothing is lost.
pub const READY_BATCH: usize = 64;

/// A stateful park store over [`block_point`], for a scheduler (e.g. the AOT native scheduler) that
/// would rather `park`/`unpark` than rebuild the park slice each idle. The interpreter derives its
/// slice from its own thread table and calls [`block_point`] directly, so it does not use this.
#[derive(Default)]
pub struct Reactor {
    parks: Vec<(u32, WaitReason)>,
}

impl Reactor {
    /// An empty park store.
    #[must_use]
    pub fn new() -> Self {
        Self { parks: Vec::new() }
    }

    /// Park `id` on `reason` (replacing any prior park for that id).
    pub fn park(&mut self, id: u32, reason: WaitReason) {
        self.parks.retain(|(existing, _)| *existing != id);
        self.parks.push((id, reason));
    }

    /// Drop `id`'s park -- it became runnable another way (a lock hand-off, a join completion).
    pub fn unpark(&mut self, id: u32) {
        self.parks.retain(|(existing, _)| *existing != id);
    }

    /// The block point over the stored parks; the woken ids are removed from the store before they
    /// are returned. `None` = deadlock (see [`block_point`]).
    #[must_use]
    pub fn block_point(&mut self, env: &mut dyn ReactorEnv) -> Option<Vec<u32>> {
        let woken = block_point(&self.parks, env)?;
        self.parks.retain(|(id, _)| !woken.contains(id));
        Some(woken)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A test env with a fixed clock and a scripted socket-ready set.
    struct FakeEnv {
        now: Option<u64>,
        slept: u64,
        ready: Vec<u32>,
        deregistered: Vec<u32>,
    }
    impl ReactorEnv for FakeEnv {
        fn now_millis(&self) -> Option<u64> {
            self.now
        }
        fn sleep_millis(&mut self, millis: u64) {
            self.slept += millis;
        }
        fn net_poll(&mut self, _timeout_ms: Option<u64>) -> Vec<u32> {
            self.ready.clone()
        }
        fn net_deregister(&mut self, socket: u32) {
            self.deregistered.push(socket);
        }
    }

    #[test]
    fn no_timer_no_io_is_deadlock() {
        let mut env = FakeEnv { now: Some(0), slept: 0, ready: vec![], deregistered: vec![] };
        assert_eq!(block_point(&[], &mut env), None);
    }

    #[test]
    fn nearest_sleeper_sets_the_timeout_and_wakes_the_due_ones() {
        let mut env = FakeEnv { now: Some(100), slept: 0, ready: vec![], deregistered: vec![] };
        let parks = vec![(1u32, WaitReason::Sleep(50)), (2u32, WaitReason::Sleep(200))];
        assert_eq!(block_point(&parks, &mut env), Some(vec![1]));
    }

    #[test]
    fn io_ready_wakes_the_waiter_and_deregisters_its_socket() {
        let mut env = FakeEnv { now: Some(0), slept: 0, ready: vec![7], deregistered: vec![] };
        let parks = vec![(3u32, WaitReason::Io(7)), (4u32, WaitReason::Io(9))];
        assert_eq!(block_point(&parks, &mut env), Some(vec![3]));
        assert_eq!(env.deregistered, vec![7]);
    }

    #[test]
    fn a_timed_io_wait_is_an_io_entry_plus_a_sleep_entry_for_the_same_thread() {
        let mut env = FakeEnv { now: Some(500), slept: 0, ready: vec![], deregistered: vec![] };
        let parks = vec![(5u32, WaitReason::Io(7)), (5u32, WaitReason::Sleep(400))];
        assert_eq!(block_point(&parks, &mut env), Some(vec![5]));
        assert_eq!(env.deregistered, Vec::<u32>::new());

        let mut both = FakeEnv { now: Some(500), slept: 0, ready: vec![7], deregistered: vec![] };
        assert_eq!(block_point(&parks, &mut both), Some(vec![5, 5]));
        assert_eq!(both.deregistered, vec![7]);
    }

    /// An env with a no-alloc `net_poll_into` and a `net_poll` that PANICS -- proving the
    /// allocation-free `block_point_into` path never routes through the Vec-returning form (the
    /// contract the AOT staticlib scheduler relies on).
    struct NoAllocEnv {
        now: Option<u64>,
        ready: Vec<u32>,
        deregistered: Vec<u32>,
    }
    impl ReactorEnv for NoAllocEnv {
        fn now_millis(&self) -> Option<u64> {
            self.now
        }
        fn sleep_millis(&mut self, _millis: u64) {}
        fn net_poll(&mut self, _timeout_ms: Option<u64>) -> Vec<u32> {
            panic!("block_point_into must use net_poll_into, never net_poll");
        }
        fn net_poll_into(&mut self, _timeout_ms: Option<u64>, ready: &mut [u32]) -> usize {
            let n = self.ready.len().min(ready.len());
            ready[..n].copy_from_slice(&self.ready[..n]);
            n
        }
        fn net_deregister(&mut self, socket: u32) {
            self.deregistered.push(socket);
        }
    }

    #[test]
    fn block_point_into_matches_block_point_without_alloc() {
        let mut env = NoAllocEnv { now: Some(100), ready: vec![], deregistered: vec![] };
        let parks = vec![(1u32, WaitReason::Sleep(50)), (2u32, WaitReason::Sleep(200))];
        let mut woken = [0u32; 8];
        assert_eq!(block_point_into(&parks, &mut env, &mut woken), Some(1));
        assert_eq!(woken[0], 1);

        let mut io_env = NoAllocEnv { now: Some(0), ready: vec![7], deregistered: vec![] };
        let io_parks = vec![(3u32, WaitReason::Io(7)), (4u32, WaitReason::Io(9))];
        assert_eq!(block_point_into(&io_parks, &mut io_env, &mut woken), Some(1));
        assert_eq!(woken[0], 3);
        assert_eq!(io_env.deregistered, vec![7]);

        let mut dead = NoAllocEnv { now: Some(0), ready: vec![], deregistered: vec![] };
        assert_eq!(block_point_into(&[], &mut dead, &mut woken), None);
    }

    #[test]
    fn stateful_reactor_removes_woken_parks() {
        let mut env = FakeEnv { now: Some(300), slept: 0, ready: vec![], deregistered: vec![] };
        let mut reactor = Reactor::new();
        reactor.park(1, WaitReason::Sleep(100));
        reactor.park(2, WaitReason::Sleep(500));
        assert_eq!(reactor.block_point(&mut env), Some(vec![1]));
        let mut env2 = FakeEnv { now: Some(600), slept: 0, ready: vec![], deregistered: vec![] };
        assert_eq!(reactor.block_point(&mut env2), Some(vec![2]));
    }
}
