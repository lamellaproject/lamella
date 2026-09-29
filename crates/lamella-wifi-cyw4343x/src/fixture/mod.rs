//! Fakes for tests against recorded exchanges: a wire that replays a script
//! of frames, a host that replays a script of commands and transfers, a
//! transport that replays a script of the trait's own operations, a
//! recorder that logs what any transport is asked, a clock the test sets,
//! and a loop that pumps the driver to an outcome while recording the wake
//! timeline.
//!
//! Every fake checks each operation it is asked against the current row of
//! its script and answers with the row's bytes; an operation that does not
//! match the row is a refusal by row number, so a driver that deviates from
//! a recorded exchange fails by name. Two more tools live here: a model of
//! the chip's bus registers and scratch RAM for the tuning's battery, and
//! the decoder that turns a recorded trace back into wire rows.

mod clock;
mod eye;
mod host;
mod record;
mod trace;
mod transport;
mod wire;

pub use clock::{End, FakeClock, Run, run};
pub use eye::{EyeWire, REFUSED, STAGE_EYE_MODEL, STAGE_EYE_REFUSED};
pub use host::{
    FakeHost, HostRow, STAGE_COMMAND_MISMATCH, STAGE_TRANSFER_MISMATCH,
    STAGE_UNEXPECTED_HOST_OPERATION,
};
pub use record::{Kind, Logged, Recorder, word_of};
pub use trace::rows_of_trace;
pub use transport::{
    FakeTransport, Op, STAGE_TRANSPORT_MISMATCH, STAGE_UNEXPECTED_TRANSPORT_OPERATION,
};
pub use wire::{
    FakeWire, Row, STAGE_FRAME_MISMATCH, STAGE_UNEXPECTED_FRAME, STAGE_UNEXPECTED_OPERATION,
};
