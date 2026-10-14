//! Reading a text file the way MSBuild and csc read one.
//!
//! **A BYTE-ORDER MARK NAMES THE ENCODING, AND IT IS NOT PART OF THE TEXT.** Windows tools write
//! one by default: Windows PowerShell 5.1's `Set-Content -Encoding utf8` writes UTF-8 with a mark,
//! and its `Out-File` writes UTF-16. MSBuild reads a project saved either way and csc compiles a
//! source saved either way, so this tool reads both. The decoding is the C# compiler's own
//! ([`decode_source`]), so a source file reads the same here as it does to `lcsc`.
//!
//! **A FILE WITH NO MARK IS UTF-8, OR IT IS REFUSED.** That is how MSBuild reads a project file.
//! csc goes on to read such a source in the machine's ANSI code page, and this tool does not:
//! the refusal says to save the file as UTF-8.

use std::path::Path;

use lamella_syntax::decode::{Encoding, decode_source};

/// What a file with no byte-order mark that is not UTF-8 is told.
const NOT_UTF8: &str = "it is not UTF-8, and no byte-order mark says it is UTF-16. Save it as UTF-8.";

/// The code page [`decode_source`] is handed. It is never read back: the one result it can
/// produce, a file decoded in a code page, is refused.
const NO_CODE_PAGE: u16 = 1252;

/// The text of the file at `path`, without its byte-order mark.
///
/// # Errors
/// A file that cannot be read, and one with no byte-order mark that is not UTF-8, each as the
/// cause alone, for the caller to put after its own words.
pub(crate) fn read_text(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    match decode_source(&bytes, NO_CODE_PAGE) {
        (text, Encoding::Utf8 | Encoding::Utf16Le | Encoding::Utf16Be) => Ok(text),
        (_, Encoding::CodePage(_)) => Err(NOT_UTF8.to_owned()),
    }
}

/// `text` as Windows tools save it, each with its byte-order mark: UTF-8 (Windows PowerShell 5.1's
/// `Set-Content -Encoding utf8`), and UTF-16 in both byte orders (its `Out-File` writes
/// little-endian).
#[cfg(test)]
pub(crate) fn as_windows_tools_save(text: &str) -> [(&'static str, Vec<u8>); 3] {
    let utf8 = [&[0xEF, 0xBB, 0xBF][..], text.as_bytes()].concat();
    let little = [0xFF, 0xFE].into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes));
    let big = [0xFE, 0xFF].into_iter().chain(text.encode_utf16().flat_map(u16::to_be_bytes));
    [
        ("UTF-8 with a byte-order mark", utf8),
        ("UTF-16 little-endian", little.collect()),
        ("UTF-16 big-endian", big.collect()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file of the test's own, holding `bytes`.
    fn written(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("lamella-text-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).expect("write the file");
        path
    }

    /// **EVERY MARK A WINDOWS TOOL WRITES READS AS THE SAME TEXT**, and so does no mark at all.
    #[test]
    fn each_byte_order_mark_reads_as_the_text_without_it() {
        let text = "class Program { static void Main() { } } // \u{e9}t\u{e9}, \u{1F600}\n";
        let plain = written("plain", text.as_bytes());
        assert_eq!(read_text(&plain).as_deref(), Ok(text), "UTF-8 with no mark");
        let _ = std::fs::remove_file(plain);
        for (index, (what, bytes)) in as_windows_tools_save(text).into_iter().enumerate() {
            let path = written(&format!("marked-{index}"), &bytes);
            assert_eq!(read_text(&path).as_deref(), Ok(text), "{what}");
            let _ = std::fs::remove_file(path);
        }
    }

    /// **NO MARK AND NOT UTF-8 IS REFUSED BY WHAT TO DO.** A file that cannot be read keeps the
    /// system's own sentence, which names the reason.
    #[test]
    fn a_file_that_is_not_utf8_and_has_no_mark_is_refused() {
        let path = written("unmarked", b"class P { } // \x93quoted\x94\n");
        assert_eq!(read_text(&path), Err(NOT_UTF8.to_owned()));
        let _ = std::fs::remove_file(&path);
        let missing = read_text(&path).expect_err("it was removed");
        assert_ne!(missing, NOT_UTF8, "a missing file is not an encoding problem");
    }
}
