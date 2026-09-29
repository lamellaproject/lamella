//! The refusal: the driver or a transport stopped at a named stage, carrying
//! the status word it saw there.

/// A named stage and the status word seen at it.
///
/// The stage is a stable string a caller can match on; the status is the
/// register reading, bus status or compared value that decided the refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The stage that refused.
    pub stage: &'static str,
    /// The status word at that stage.
    pub status: u32,
}

impl Refusal {
    /// A refusal at `stage` with `status`.
    pub const fn new(stage: &'static str, status: u32) -> Self {
        Refusal { stage, status }
    }
}
