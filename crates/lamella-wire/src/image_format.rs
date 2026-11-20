//! What a host and a target say when an image's format is not the one a runtime loads.
//!
//! An image carries one format number, and a runtime loads exactly one format, which its handshake
//! states ([`crate::HelloAck::image_formats`]). A host refuses before it writes an image of another
//! format, and a board refuses at boot an image of another format. Each answer is one sentence,
//! kept here so every host and every firmware says the same words.
//!
//! A format number is 16 bits: the [`PRE_RELEASE`] flag in bit 15, and a count in bits 0 to 14. A
//! pre-release format is older than every production format, and between two formats that agree
//! on the flag the higher count is the newer. Only which answer a host gives depends on that order;
//! whether an image is written is always one number compared with one number.

/// The flag a pre-release format carries in bit 15 of its number; a production format has it clear.
pub const PRE_RELEASE: u16 = 0x8000;

/// The format a runtime that loads no image states.
pub const LOADS_NONE: u16 = 0;

/// A host's answer when the board's runtime loads an older format than the image's, or states none.
pub const UPDATE_FIRMWARE: &str = "Update the board's firmware first.";

/// A host's answer when the board's runtime loads a newer format than the image's.
pub const UPDATE_LAMELLA: &str = "Update Lamella to match the board's firmware.";

/// A host's answer when an image file's format is not the one this host writes.
pub const REBUILD_IMAGE: &str = "Rebuild the image with this Lamella.";

/// A board's answer, at boot, to an image whose format is not the one it loads.
pub const REDEPLOY_PROGRAM: &str = "Redeploy the program with a matching Lamella.";

/// Whether the format `a` is newer than the format `b`: a production format is newer than every
/// pre-release one ([`PRE_RELEASE`]), and between two that agree on the flag the higher count is
/// newer.
#[must_use]
pub fn is_newer(a: u16, b: u16) -> bool {
    let pre_release = |format: u16| format & PRE_RELEASE != 0;
    match (pre_release(a), pre_release(b)) {
        (false, true) => true,
        (true, false) => false,
        _ => a & !PRE_RELEASE > b & !PRE_RELEASE,
    }
}

/// Why a host must not write an image of the format `image` to a runtime whose handshake stated the
/// format `board`, or `None` when the two are the same format. A runtime that stated no format
/// (`None`), or [`LOADS_NONE`], loads none this host writes, whatever the image's tag says.
#[must_use]
pub fn refusal(board: Option<u16>, image: u16) -> Option<&'static str> {
    match board {
        None | Some(LOADS_NONE) => Some(UPDATE_FIRMWARE),
        Some(board) if board == image => None,
        Some(board) if is_newer(board, image) => Some(UPDATE_LAMELLA),
        Some(_) => Some(UPDATE_FIRMWARE),
    }
}

/// Why a host must not send an image file of the format `image` when the format it writes is `own`,
/// or `None` when they are the same format. A file whose format could not be read (`None`) is not of
/// the format this host writes.
#[must_use]
pub fn file_refusal(image: Option<u32>, own: u16) -> Option<&'static str> {
    (image != Some(u32::from(own))).then_some(REBUILD_IMAGE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST_PRE_RELEASE: u16 = PRE_RELEASE | 1;
    const FIRST_PRODUCTION: u16 = 1;

    #[test]
    fn each_answer_is_one_sentence_of_eight_words_or_fewer() {
        for text in [UPDATE_FIRMWARE, UPDATE_LAMELLA, REBUILD_IMAGE, REDEPLOY_PROGRAM] {
            assert!(text.split_whitespace().count() <= 8, "{text}");
            assert!(text.ends_with('.') && text.matches('.').count() == 1, "{text}");
        }
    }

    #[test]
    fn a_production_format_is_newer_than_every_pre_release_one() {
        assert!(is_newer(FIRST_PRODUCTION, FIRST_PRE_RELEASE));
        assert!(!is_newer(FIRST_PRE_RELEASE, FIRST_PRODUCTION));
        assert!(is_newer(PRE_RELEASE | 2, FIRST_PRE_RELEASE));
        assert!(!is_newer(FIRST_PRE_RELEASE, PRE_RELEASE | 2));
        assert!(is_newer(FIRST_PRODUCTION + 1, FIRST_PRODUCTION));
        assert!(is_newer(FIRST_PRODUCTION, PRE_RELEASE | 0x7FFF), "the highest pre-release count too");
        assert!(!is_newer(FIRST_PRE_RELEASE, FIRST_PRE_RELEASE));
    }

    #[test]
    fn a_host_writes_only_the_format_the_board_states() {
        assert_eq!(refusal(Some(FIRST_PRE_RELEASE), FIRST_PRE_RELEASE), None);
        assert_eq!(refusal(None, FIRST_PRE_RELEASE), Some(UPDATE_FIRMWARE), "a board stating none");
        assert_eq!(refusal(Some(LOADS_NONE), FIRST_PRE_RELEASE), Some(UPDATE_FIRMWARE));
        assert_eq!(refusal(Some(LOADS_NONE), LOADS_NONE), Some(UPDATE_FIRMWARE), "a runtime loading none, whatever the tag");
        assert_eq!(refusal(None, LOADS_NONE), Some(UPDATE_FIRMWARE));
        assert_eq!(refusal(Some(FIRST_PRE_RELEASE), PRE_RELEASE | 2), Some(UPDATE_FIRMWARE));
        assert_eq!(refusal(Some(PRE_RELEASE | 2), FIRST_PRE_RELEASE), Some(UPDATE_LAMELLA));
        assert_eq!(refusal(Some(PRE_RELEASE | 31), PRE_RELEASE | 31), None);
    }

    #[test]
    fn across_the_release_a_production_board_is_newer_and_a_pre_release_board_older() {
        assert_eq!(refusal(Some(FIRST_PRODUCTION), FIRST_PRE_RELEASE), Some(UPDATE_LAMELLA));
        assert_eq!(refusal(Some(FIRST_PRE_RELEASE), FIRST_PRODUCTION), Some(UPDATE_FIRMWARE));
        assert_eq!(refusal(Some(FIRST_PRODUCTION), FIRST_PRODUCTION), None);
    }

    #[test]
    fn a_host_sends_only_an_image_file_of_its_own_format() {
        assert_eq!(file_refusal(Some(u32::from(FIRST_PRE_RELEASE)), FIRST_PRE_RELEASE), None);
        assert_eq!(file_refusal(Some(1), FIRST_PRE_RELEASE), Some(REBUILD_IMAGE), "a 0.5.0 image says 1");
        assert_eq!(file_refusal(Some(u32::from(PRE_RELEASE | 2)), FIRST_PRE_RELEASE), Some(REBUILD_IMAGE));
        assert_eq!(file_refusal(None, FIRST_PRE_RELEASE), Some(REBUILD_IMAGE));
        assert_eq!(file_refusal(Some(0x1_0000 | u32::from(FIRST_PRE_RELEASE)), FIRST_PRE_RELEASE), Some(REBUILD_IMAGE));
    }
}
