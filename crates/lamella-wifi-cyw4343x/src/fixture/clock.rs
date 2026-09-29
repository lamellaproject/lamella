//! The clock a test sets, and the loop that pumps the driver to an outcome
//! while recording the wake timeline.

use crate::clock::{Clock, Micros};
use crate::driver::{Driver, Outcome, Wake};
use crate::transport::Transport;

/// A clock the test sets.
#[derive(Clone, Copy, Debug, Default)]
pub struct FakeClock {
    now: Micros,
}

impl FakeClock {
    /// A clock reading `now`.
    pub const fn new(now: Micros) -> Self {
        FakeClock { now }
    }

    /// Set the clock to `now`, never backwards.
    pub fn set(&mut self, now: Micros) {
        if now > self.now {
            self.now = now;
        }
    }

    /// The clock's reading.
    pub fn now(&self) -> Micros {
        self.now
    }
}

impl Clock for FakeClock {
    fn now_us(&mut self) -> Micros {
        self.now
    }

    fn delay_us(&mut self, us: u32) {
        self.now += Micros::from(us);
    }
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// An outcome was returned.
    Done,
    /// The driver went idle with no outcome.
    Idle,
    /// The poll cap was reached.
    Cap,
}

const MAX_DEADLINES: usize = 2048;

/// A run's record: the outcome, the poll count and the wake timeline.
#[derive(Clone, Copy, Debug)]
pub struct Run {
    /// The outcome, if the run ended in one.
    pub outcome: Option<Outcome>,
    /// How the run ended.
    pub end: End,
    /// The number of polls.
    pub polls: u32,
    deadlines: [Micros; MAX_DEADLINES],
    n_deadlines: usize,
}

impl Run {
    /// The deadlines the driver asked for, in order (the first 2,048).
    pub fn deadlines(&self) -> &[Micros] {
        &self.deadlines[..self.n_deadlines]
    }
}

/// Pump `driver` until an outcome, an idle, or `cap` polls, setting the
/// clock forward to each deadline the driver asks for.
pub fn run<T: Transport>(driver: &mut Driver, bus: &mut T, clock: &mut FakeClock, cap: u32) -> Run {
    let mut run = Run {
        outcome: None,
        end: End::Cap,
        polls: 0,
        deadlines: [0; MAX_DEADLINES],
        n_deadlines: 0,
    };
    while run.polls < cap {
        run.polls += 1;
        match driver.poll(bus, clock) {
            Wake::Again => {}
            Wake::At(t) | Wake::Irq(t) => {
                if run.n_deadlines < MAX_DEADLINES {
                    run.deadlines[run.n_deadlines] = t;
                    run.n_deadlines += 1;
                }
                clock.set(t);
            }
            Wake::Idle => {
                run.end = End::Idle;
                return run;
            }
            Wake::Done(outcome) => {
                run.outcome = Some(outcome);
                run.end = End::Done;
                return run;
            }
        }
    }
    run
}
