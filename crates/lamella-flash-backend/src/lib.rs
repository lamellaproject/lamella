//! The contract every board flashing backend implements, and the driver that owns the ORDER.

#![forbid(unsafe_code)]

use lamella_probe_core::ProbeError;

/// An image that is ready to be written: flat bytes and the address they belong at.
///
/// **RESOLVED BEFORE IT REACHES A BACKEND.** Whatever the file was -- a raw binary, Intel HEX,
/// Motorola S-records, a linked ELF -- it arrives here as one contiguous span, because that is what
/// a flash writer takes and because a backend that parsed formats would be a second place formats
/// are understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image<'a> {
    /// The bytes, contiguous from [`base`](Self::base).
    pub bytes: &'a [u8],
    /// The address the first byte belongs at.
    pub base: u32,
}

impl Image<'_> {
    /// The address one past the last byte.
    #[must_use]
    pub fn end(&self) -> u32 {
        self.base.saturating_add(u32::try_from(self.bytes.len()).unwrap_or(u32::MAX))
    }
}

/// What a part answered when asked what it is, before anything was erased.
///
/// **A `value` alone does not identify a BOARD.** Debug-port ids are commonly family-wide -- two
/// boards differing only in fitted flash answer identically -- so `what` names what the reading can
/// actually settle, and a backend that needs to tell two boards apart has to read something else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartIdentity {
    /// The raw reading, for the message when it disagrees.
    ///
    pub value: u64,
    /// What this reading settles, in words a person can act on.
    ///
    /// **NOT "WHICH BOARD THIS IS", UNLESS IT REALLY IS.** A debug-port id is usually shared by a
    /// whole family, and a caller deciding whether it may write THIS board needs to know which
    /// kind of answer it is holding.
    pub what: &'static str,
}

/// Why a flash attempt stopped.
#[derive(Debug)]
pub enum FlashError {
    /// A probe or debug-access failure.
    Probe(ProbeError),
    /// The part is not the one this image is for. **Raised before any erase.**
    WrongPart {
        /// What the board's facts said to expect.
        expected: PartIdentity,
        /// What the target actually answered.
        found: u64,
    },
    /// The image does not belong where this board is written from.
    WrongBase {
        /// The address the image states.
        stated: u32,
        /// The address this board is written from.
        expected: u32,
    },
    /// A byte did not read back.
    Verify {
        /// Address of the first byte that differed.
        address: u32,
        /// What the image says belongs there.
        expected: u8,
        /// What the part answered.
        got: u8,
    },
    /// The read-back returned a different number of bytes than were written, so the comparison
    /// could not be made at all.
    ///
    /// **Its own variant rather than a [`Verify`](Self::Verify) at the first missing byte**,
    /// because a short read is a broken instrument and a mismatch is a broken write, and reporting
    /// one as the other sends the reader to the wrong part of the system.
    ShortReadBack {
        /// How many bytes were written.
        wrote: usize,
        /// How many came back.
        read: usize,
    },
    /// The part is not one this caller was permitted to write. **Raised before any erase.**
    ///
    /// Distinct from [`WrongPart`](Self::WrongPart), which means the image does not fit the part.
    /// This one means the part was correctly identified and the caller may not touch it.
    NotAllowed {
        /// What the part said it was.
        found: PartIdentity,
    },
    /// The backend refused for a reason only it can describe.
    Refused(String),
}

impl From<ProbeError> for FlashError {
    fn from(error: ProbeError) -> Self {
        FlashError::Probe(error)
    }
}

impl core::fmt::Display for FlashError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FlashError::Probe(error) => write!(f, "{error:?}"),
            FlashError::WrongPart { expected, found } => write!(
                f,
                "this board answered {found:#010x} and the image is for {:#010x} ({}). \
                 Nothing was erased.",
                expected.value, expected.what
            ),
            FlashError::WrongBase { stated, expected } => write!(
                f,
                "this image states it belongs at {stated:#010x} and this board is written from \
                 {expected:#010x}. It was almost certainly built for a different part."
            ),
            FlashError::Verify { address, expected, got } => write!(
                f,
                "the byte at {address:#010x} reads back {got:#04x} where the image says \
                 {expected:#04x}: the write did not take."
            ),
            FlashError::ShortReadBack { wrote, read } => write!(
                f,
                "wrote {wrote} bytes and read back {read}, so nothing was compared. This is a \
                 broken read, not a failed write."
            ),
            FlashError::NotAllowed { found } => write!(
                f,
                "this part reads as {:#x} -- {} -- and this caller is not permitted to write it.\
\n\
Nothing was erased.",
                found.value, found.what
            ),
            FlashError::Refused(why) => write!(f, "{why}"),
        }
    }
}

/// Whether the bytes on the part were checked against the image, and how.
///
/// **THREE STATES, NOT TWO, AND THE THIRD IS THE POINT.** A route that cannot read back is not a
/// route that failed to verify: collapsing them makes a tool claim a check it never ran, which is
/// the failure this type exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// Every byte was read back and matched.
    ReadBack,
    /// This MECHANISM cannot read the part back. What the write got instead, in words a person can
    /// act on -- a bootloader's own admission check, say.
    NotPossible(&'static str),
    /// A read-back was possible and the caller asked for it to be skipped.
    Skipped,
}

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The mechanism, for a person reading the output.
    pub mechanism: &'static str,
    /// What the part said it was.
    pub identity: PartIdentity,
    /// Where the image was written.
    pub base: u32,
    /// How many bytes.
    pub bytes: usize,
    /// Whether those bytes were checked, and how.
    pub verification: Verification,
}

/// Which physical parts a caller is permitted to write.
///
/// **CHECKED AFTER `identify` AND BEFORE `erase`, WHICH IS THE ONLY PLACE IT MEANS ANYTHING.** A
/// permission checked against what the CALLER ASKED FOR -- a board model, a probe serial -- is a
/// check on the request. This is a check on the DEVICE: it compares the reading the part itself
/// gave against a list, at the one moment when the part has been identified and nothing has been
/// destroyed.
///
/// That distinction is the whole point. A probe serial names a cable, and a cable can be moved to
/// another board between one run and the next; a board model names a whole family. Neither is an
/// answer to "may I erase THIS".
///
/// It is only as strong as the backend's [`identify`](FlashBackend::identify): a part whose
/// identity is a family-wide debug-port id cannot be pinned to one board by any list, and
/// [`PartIdentity::what`] is what tells a caller which kind of answer it is holding.
///
/// **A GUARD AGAINST MISTAKES, NOT A SECURITY BOUNDARY.** It binds the caller that passes it and
/// nothing else: whoever can construct an `Allow` can construct [`Allow::Any`], and a process that
/// can reach a probe at all can reach it by another path. What this prevents is a write aimed at
/// the wrong board. What it cannot prevent is a caller that meant to aim there.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Allow {
    /// Any part this build knows how to write.
    #[default]
    Any,
    /// Only parts whose identity reads as one of these.
    Identities(Vec<u64>),
}

impl Allow {
    /// Whether `identity` is permitted.
    ///
    /// **AS NARROW AS THE READING BEHIND IT AND NO NARROWER**, and that is not the same for every
    /// part -- see [`PartIdentity`]. Where the value is a family or category id, a permission that
    /// reads like "this one board" is really "any part that answers this". A caller that offers this
    /// guard to a person has to tell them which of the two they have, and [`PartIdentity::what`] is
    /// the field that knows.
    #[must_use]
    pub fn permits(&self, identity: &PartIdentity) -> bool {
        match self {
            Allow::Any => true,
            Allow::Identities(allowed) => allowed.contains(&identity.value),
        }
    }
}

/// Whether to read the written image back where the mechanism allows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyPolicy {
    /// Read every byte back and compare. The default, because a verify that is optional by default
    /// is a verify most callers never run.
    #[default]
    ReadBack,
    /// Do not read back, even though this mechanism could.
    ///
    /// Reported as [`Verification::Skipped`] rather than silently resembling a verified write.
    Skip,
}

/// One part's flashing steps. **The order they run in is not defined here** -- see [`flash`].
pub trait FlashBackend {
    /// What this mechanism is, for a person reading the output. A phrase, not a noun: it is
    /// rendered into a sentence about what is happening to their board.
    fn mechanism(&self) -> &'static str;

    /// The address this mechanism writes an image from.
    ///
    /// **ONE PLACE STATES IT**, so the address a build declares is the address a write uses.
    fn flash_base(&self) -> u32;

    /// Ask the part what it is, touching nothing.
    ///
    /// Called BEFORE any erase, and a disagreement is terminal. An implementation that cannot
    /// distinguish the board it is pointed at from a sibling must say what its reading actually
    /// settles in [`PartIdentity::what`] rather than implying more.
    ///
    /// # Errors
    /// When the target cannot be reached or does not answer.
    fn identify(&mut self) -> Result<PartIdentity, FlashError>;

    /// Prepare the part and erase exactly the span `image` covers.
    ///
    /// Halting, unlocking and re-locking belong here: they are part-specific, and a caller that
    /// had to remember them would be a caller that forgets one.
    ///
    /// # Errors
    /// When the erase does not complete.
    fn erase(&mut self, image: &Image<'_>) -> Result<(), FlashError>;

    /// Write `image` to the part.
    ///
    /// # Errors
    /// When the programming operation fails or the controller reports an error.
    fn program(&mut self, image: &Image<'_>) -> Result<(), FlashError>;

    /// Read back the span `image` covers, or `None` if this MECHANISM cannot.
    ///
    /// **`None` IS A STATEMENT ABOUT THE MECHANISM, NOT ABOUT THIS ATTEMPT.** Answer it only where
    /// no read-back exists at all -- a bootloader volume that unmounts when the board reboots. The
    /// `&'static str` is what the write got INSTEAD, and it is reported to the user, so it must be
    /// true and specific: naming a check the mechanism really performs is useful, and naming a
    /// vague reassurance is worse than saying nothing.
    ///
    /// A read that FAILS is `Some(Err(..))`. Returning `None` because a read went wrong would
    /// turn a broken probe into a permanent property of the mechanism.
    ///
    /// # Errors
    /// When the read-back itself fails.
    fn read_back(&mut self, image: &Image<'_>) -> Option<Result<Vec<u8>, FlashError>>;

    /// Leave the part running the image.
    ///
    /// # Errors
    /// When the part cannot be released.
    fn finish(&mut self) -> Result<(), FlashError>;
}

/// Write `image` through `backend`, in the order the contract guarantees.
///
/// **THIS FUNCTION IS THE CONTRACT'S ENFORCEMENT, AND EVERY STEP IS ORDERED FOR A MEASURED
/// REASON.** Identify, check the base, erase, program, read back, finish. A backend cannot reorder
/// these because it is not asked to.
///
/// The base check and the identity check both run BEFORE the erase. A guard after an erase has
/// already destroyed what it was protecting.
///
/// # Errors
/// The first step that fails, with nothing after it attempted.
pub fn flash(
    backend: &mut impl FlashBackend,
    image: &Image<'_>,
    policy: VerifyPolicy,
    allow: &Allow,
) -> Result<Report, FlashError> {
    if image.base != backend.flash_base() {
        return Err(FlashError::WrongBase { stated: image.base, expected: backend.flash_base() });
    }
    let identity = backend.identify()?;
    if !allow.permits(&identity) {
        return Err(FlashError::NotAllowed { found: identity });
    }

    backend.erase(image)?;
    backend.program(image)?;

    let verification = match policy {
        VerifyPolicy::Skip => Verification::Skipped,
        VerifyPolicy::ReadBack => match backend.read_back(image) {
            None => Verification::NotPossible(instead_of_read_back(backend)),
            Some(read) => {
                let read = read?;
                compare(image, &read)?;
                Verification::ReadBack
            }
        },
    };

    backend.finish()?;
    Ok(Report {
        mechanism: backend.mechanism(),
        identity,
        base: image.base,
        bytes: image.bytes.len(),
        verification,
    })
}

/// What a mechanism that cannot read back offers instead.
///
fn instead_of_read_back(backend: &impl FlashBackend) -> &'static str {
    backend.mechanism()
}

/// Compare a read-back against the image, reporting the FIRST byte that differs.
///
/// A short read is its own error rather than a mismatch at the first missing byte: one is a
/// broken instrument and the other a broken write, and they send a reader to different places.
fn compare(image: &Image<'_>, read: &[u8]) -> Result<(), FlashError> {
    if read.len() != image.bytes.len() {
        return Err(FlashError::ShortReadBack { wrote: image.bytes.len(), read: read.len() });
    }
    for (offset, (expected, got)) in image.bytes.iter().zip(read.iter()).enumerate() {
        if expected != got {
            return Err(FlashError::Verify {
                address: image.base.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX)),
                expected: *expected,
                got: *got,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
