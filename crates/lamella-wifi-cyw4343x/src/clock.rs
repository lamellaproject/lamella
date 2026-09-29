//! The caller's clock: monotonic microseconds, and a short busy wait.

/// Microseconds on the caller's monotonic clock. The origin is the caller's;
/// the driver only ever compares instants and adds budgets to them.
pub type Micros = u64;

/// The clock the caller passes into every `poll`.
///
/// `now_us` is read once per call and compared against deadlines the driver
/// recorded earlier. `delay_us` is the one place the driver waits inside a
/// call: the microsecond-scale settles of the chip's core reset sequences.
/// The driver asks for at most 100 microseconds in one call, so a caller may
/// implement it as a spin on its own clock.
pub trait Clock {
    /// Monotonic microseconds since any fixed origin.
    fn now_us(&mut self) -> Micros;

    /// A bounded busy wait of `us` microseconds; never asked for more than
    /// 100 in one call.
    fn delay_us(&mut self, us: u32);
}
