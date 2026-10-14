//! csc's response files: an `@file` argument stands for the arguments written in that file.
//!
//! The rules are csc's: as its documentation states them, and where the documentation is silent,
//! as csc behaves. The options and sources in the file are processed as if they had been given on
//! the command line where the `@file` stands, so an option after it overrides one inside it, and
//! the other way round. Several arguments may share a line, and none
//! spans two: a line ends at CR LF, LF or CR, and a quote still open there closes with it.
//! Arguments are separated by spaces and tabs. A double quote groups what it encloses into one
//! argument and may open and close inside one; `\"` is a literal quote; backslashes are literal
//! except before a quote, where each pair stands for one backslash and an odd one left over makes
//! the quote literal. A `#` where an argument would begin starts a comment that runs to the end of
//! its line, and anywhere else it is an ordinary character. An `@file` inside a response file is
//! read in its turn, and a relative path, a response file's own included, is taken from the
//! current directory, never from the directory of the response file that names it.
//!
//! A file that cannot be opened is csc's CS2011, and nothing is compiled. A response file that
//! includes itself, directly or through another, is refused by name.

use std::fs;
use std::path::{Path, PathBuf};

/// Replaces every argument that begins with `@` by the arguments its response file holds, in place
/// and recursively. Relative paths are taken from `base`, which the driver passes as the current
/// directory. The error is the message to print.
pub(crate) fn expand(args: &[String], base: &Path) -> Result<Vec<String>, String> {
    let mut expanded = Vec::with_capacity(args.len());
    let mut open = Vec::new();
    for arg in args {
        expand_argument(arg, base, &mut open, &mut expanded)?;
    }
    Ok(expanded)
}

/// A response file being read, as its path is shown and as the file it is.
struct OpenFile {
    shown: PathBuf,
    identity: PathBuf,
}

/// Appends `arg` to `expanded`, or, when it names a response file, the arguments that file holds.
/// `open` lists the response files being read around this one, outermost first.
fn expand_argument(
    arg: &str,
    base: &Path,
    open: &mut Vec<OpenFile>,
    expanded: &mut Vec<String>,
) -> Result<(), String> {
    let Some(name) = arg.strip_prefix('@') else {
        expanded.push(arg.to_owned());
        return Ok(());
    };
    if name.is_empty() {
        return Err(String::from(
            "CS2021: File name '' is empty, contains invalid characters, has a drive specification without an absolute path, or is too long",
        ));
    }
    let shown = base.join(name);
    let bytes = fs::read(&shown)
        .map_err(|_| format!("CS2011: Error opening response file '{}'", shown.display()))?;
    let identity = fs::canonicalize(&shown).unwrap_or_else(|_| shown.clone());
    if let Some(start) = open.iter().position(|file| file.identity == identity) {
        let chain: Vec<String> = open[start..]
            .iter()
            .map(|file| file.shown.display().to_string())
            .chain(core::iter::once(shown.display().to_string()))
            .collect();
        return Err(format!(
            "response file '{}' includes itself, so it can never be read to its end: {}",
            shown.display(),
            chain.join(" -> ")
        ));
    }
    open.push(OpenFile { shown, identity });
    for line in decode(&bytes).split(['\r', '\n']) {
        for argument in split_line(line) {
            expand_argument(&argument, base, open, expanded)?;
        }
    }
    open.pop();
    Ok(())
}

/// Splits one line of a response file into its arguments, by the rules in this module's summary.
fn split_line(line: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|&c| c == ' ' || c == '\t').is_some() {}
        match chars.peek() {
            None | Some('#') => return arguments,
            Some(_) => {}
        }
        let mut argument = String::new();
        let mut quoted = false;
        while let Some(c) = chars.next_if(|&c| quoted || (c != ' ' && c != '\t')) {
            match c {
                '\\' => {
                    let mut backslashes = 1;
                    while chars.next_if_eq(&'\\').is_some() {
                        backslashes += 1;
                    }
                    if chars.peek() == Some(&'"') {
                        argument.extend(core::iter::repeat_n('\\', backslashes / 2));
                        if backslashes % 2 == 1 {
                            chars.next();
                            argument.push('"');
                        }
                    } else {
                        argument.extend(core::iter::repeat_n('\\', backslashes));
                    }
                }
                '"' => quoted = !quoted,
                _ => argument.push(c),
            }
        }
        if !argument.is_empty() {
            arguments.push(argument);
        }
    }
}

/// The text of a response file: by its byte order mark when it has one (UTF-8, or UTF-16 or
/// UTF-32 in either byte order), and as UTF-8 otherwise. A malformed sequence, or a unit cut short
/// at the end of the file, reads as U+FFFD.
fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        String::from_utf8_lossy(rest).into_owned()
    } else if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE\0\0") {
        decode_utf32(rest, u32::from_le_bytes)
    } else if let Some(rest) = bytes.strip_prefix(b"\0\0\xFE\xFF") {
        decode_utf32(rest, u32::from_be_bytes)
    } else if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
        decode_utf16(rest, u16::from_le_bytes)
    } else if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
        decode_utf16(rest, u16::from_be_bytes)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let units = bytes.chunks_exact(2).map(|pair| unit([pair[0], pair[1]]));
    let mut text: String = char::decode_utf16(units)
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    if !bytes.len().is_multiple_of(2) {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

fn decode_utf32(bytes: &[u8], unit: fn([u8; 4]) -> u32) -> String {
    let mut text: String = bytes
        .chunks_exact(4)
        .map(|quad| {
            char::from_u32(unit([quad[0], quad[1], quad[2], quad[3]]))
                .unwrap_or(char::REPLACEMENT_CHARACTER)
        })
        .collect();
    if !bytes.len().is_multiple_of(4) {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A fresh directory of this test's own, so tests running side by side never share a file.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lcsc-rsp-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| String::from(*item)).collect()
    }

    /// What one response file holding `bytes` expands to.
    fn expand_file(name: &str, bytes: &[u8]) -> Vec<String> {
        let dir = scratch(name);
        fs::write(dir.join("a.rsp"), bytes).unwrap();
        let expanded = expand(&strings(&["@a.rsp"]), &dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        expanded
    }

    #[test]
    fn an_at_argument_is_replaced_in_place_by_the_arguments_its_file_holds() {
        let dir = scratch("in-place");
        fs::write(dir.join("list.rsp"), "a.cs b.cs\n/unsafe\n").unwrap();
        let args = strings(&["/out:x.dll", "@list.rsp", "c.cs"]);
        assert_eq!(
            expand(&args, &dir).unwrap(),
            strings(&["/out:x.dll", "a.cs", "b.cs", "/unsafe", "c.cs"])
        );
        fs::write(dir.join("two.rsp"), "d.cs\n").unwrap();
        assert_eq!(
            expand(&strings(&["@list.rsp", "@two.rsp"]), &dir).unwrap(),
            strings(&["a.cs", "b.cs", "/unsafe", "d.cs"])
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spaces_and_tabs_separate_and_quotes_group_as_cscs_documented_rules_say() {
        let cases: &[(&str, &[&str])] = &[
            ("a.cs b.cs", &["a.cs", "b.cs"]),
            ("a.cs\tb.cs", &["a.cs", "b.cs"]),
            ("  a.cs   b.cs  ", &["a.cs", "b.cs"]),
            (r#""d 2\s.cs""#, &[r"d 2\s.cs"]),
            (r#"d" "2\s.cs"#, &[r"d 2\s.cs"]),
            (r#"q\"q.cs"#, &[r#"q"q.cs"#]),
            (r"d1\x.cs", &[r"d1\x.cs"]),
            (r"\\server\share\x.cs", &[r"\\server\share\x.cs"]),
            (r#"d1\\"x.cs""#, &[r"d1\x.cs"]),
            (r#"d1\\\"x.cs"#, &[r#"d1\"x.cs"#]),
            (r#""d 2\\"s.cs"#, &[r"d 2\s.cs"]),
            (r#""a""b.cs""#, &["ab.cs"]),
            (r#"a.cs "" b.cs"#, &["a.cs", "b.cs"]),
            ("a.cs\u{A0}b.cs", &["a.cs\u{A0}b.cs"]),
            ("a.cs\u{2028}b.cs", &["a.cs\u{2028}b.cs"]),
            ("", &[]),
        ];
        for (index, (line, expected)) in cases.iter().enumerate() {
            let name = format!("split-{index}");
            assert_eq!(
                expand_file(&name, line.as_bytes()),
                strings(expected),
                "{line:?}"
            );
        }
    }

    #[test]
    fn a_hash_where_an_argument_would_begin_starts_a_comment_and_nowhere_else() {
        let cases: &[(&str, &[&str])] = &[
            ("# build the first output file", &[]),
            ("   # an indented comment b.cs", &[]),
            ("a.cs # b.cs", &["a.cs"]),
            ("a.cs\t#b.cs", &["a.cs"]),
            ("a.cs#b.cs", &["a.cs#b.cs"]),
            ("/define:FOO#BAR", &["/define:FOO#BAR"]),
            (r##""#h.cs""##, &["#h.cs"]),
            (r##""a.cs"#b.cs"##, &["a.cs#b.cs"]),
            ("#comment\nb.cs", &["b.cs"]),
        ];
        for (index, (line, expected)) in cases.iter().enumerate() {
            let name = format!("hash-{index}");
            assert_eq!(
                expand_file(&name, line.as_bytes()),
                strings(expected),
                "{line:?}"
            );
        }
        let dir = scratch("hash-command-line");
        assert_eq!(
            expand(&strings(&["#h.cs"]), &dir).unwrap(),
            strings(&["#h.cs"])
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_ends_at_cr_lf_lf_or_cr_and_a_quote_still_open_closes_with_it() {
        assert_eq!(
            expand_file("lines-crlf", b"a.cs\r\n\r\nb.cs\r\n"),
            strings(&["a.cs", "b.cs"])
        );
        assert_eq!(
            expand_file("lines-cr", b"a.cs\rb.cs\r"),
            strings(&["a.cs", "b.cs"])
        );
        assert_eq!(
            expand_file("lines-lf-quoted", b"\"a.cs\nb.cs\""),
            strings(&["a.cs", "b.cs"])
        );
        assert_eq!(
            expand_file("lines-cr-quoted", b"\"a.cs\rb.cs\""),
            strings(&["a.cs", "b.cs"])
        );
        assert_eq!(expand_file("lines-unclosed", b"\"a.cs"), strings(&["a.cs"]));
        assert_eq!(expand_file("lines-blank", b"  \n\t\n"), strings(&[]));
    }

    #[test]
    fn the_byte_order_mark_names_the_encoding_and_utf8_is_read_without_one() {
        let text = "\u{E9}.cs\n";
        let mut utf8_bom = b"\xEF\xBB\xBF".to_vec();
        utf8_bom.extend_from_slice(text.as_bytes());
        let mut utf16le = b"\xFF\xFE".to_vec();
        utf16le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let mut utf16be = b"\xFE\xFF".to_vec();
        utf16be.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        let mut utf32le = b"\xFF\xFE\0\0".to_vec();
        utf32le.extend(text.chars().flat_map(|c| u32::from(c).to_le_bytes()));
        let mut utf32be = b"\0\0\xFE\xFF".to_vec();
        utf32be.extend(text.chars().flat_map(|c| u32::from(c).to_be_bytes()));
        for (name, bytes) in [
            ("bom-none", text.as_bytes().to_vec()),
            ("bom-utf8", utf8_bom),
            ("bom-utf16le", utf16le),
            ("bom-utf16be", utf16be),
            ("bom-utf32le", utf32le),
            ("bom-utf32be", utf32be),
        ] {
            assert_eq!(expand_file(name, &bytes), strings(&["\u{E9}.cs"]), "{name}");
        }
        assert_eq!(
            expand_file("bom-none-latin1", b"\xE9.cs\n"),
            strings(&["\u{FFFD}.cs"])
        );
        assert_eq!(
            expand_file("bom-utf16-odd", b"\xFF\xFEa\0b"),
            strings(&["a\u{FFFD}"])
        );
        assert_eq!(
            expand_file("bom-utf32-short", b"\xFF\xFE\0\0a\0\0\0b"),
            strings(&["a\u{FFFD}"])
        );
    }

    #[test]
    fn an_at_begins_a_response_file_only_at_the_start_of_an_argument_with_its_quotes_removed() {
        let dir = scratch("at-start");
        fs::write(dir.join("inner.rsp"), "b.cs\n").unwrap();
        assert_eq!(
            expand(&strings(&["x@inner.rsp"]), &dir).unwrap(),
            strings(&["x@inner.rsp"])
        );
        fs::write(dir.join("outer.rsp"), "\"@inner.rsp\" \"/unsafe+\"\n").unwrap();
        assert_eq!(
            expand(&strings(&["@outer.rsp"]), &dir).unwrap(),
            strings(&["b.cs", "/unsafe+"])
        );
        fs::create_dir_all(dir.join("d 2")).unwrap();
        fs::write(dir.join("d 2").join("in.rsp"), "c.cs\n").unwrap();
        fs::write(dir.join("spaced.rsp"), "@\"d 2/in.rsp\"\n").unwrap();
        assert_eq!(
            expand(&strings(&["@spaced.rsp"]), &dir).unwrap(),
            strings(&["c.cs"])
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_nested_response_file_is_read_and_relative_paths_come_from_the_current_directory() {
        let dir = scratch("nested");
        fs::create_dir_all(dir.join("d1")).unwrap();
        fs::write(dir.join("outer.rsp"), "@inner.rsp\n").unwrap();
        fs::write(dir.join("inner.rsp"), "b.cs\n").unwrap();
        assert_eq!(
            expand(&strings(&["@outer.rsp"]), &dir).unwrap(),
            strings(&["b.cs"])
        );
        fs::write(dir.join("d1").join("outer.rsp"), "@inner2.rsp x.cs\n").unwrap();
        fs::write(dir.join("inner2.rsp"), "a.cs\n").unwrap();
        fs::write(dir.join("d1").join("inner2.rsp"), "c.cs\n").unwrap();
        assert_eq!(
            expand(&strings(&["@d1/outer.rsp"]), &dir).unwrap(),
            strings(&["a.cs", "x.cs"])
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_response_file_that_cannot_be_opened_is_cscs_cs2011_naming_the_path_it_tried() {
        let dir = scratch("missing");
        fs::create_dir_all(dir.join("d1")).unwrap();
        let cs2011 =
            |path: PathBuf| format!("CS2011: Error opening response file '{}'", path.display());
        assert_eq!(
            expand(&strings(&["@nope.rsp"]), &dir),
            Err(cs2011(dir.join("nope.rsp")))
        );
        assert_eq!(
            expand(&strings(&["a.cs", "@nope.rsp"]), &dir),
            Err(cs2011(dir.join("nope.rsp")))
        );
        assert_eq!(
            expand(&strings(&["@d1"]), &dir),
            Err(cs2011(dir.join("d1")))
        );
        fs::write(dir.join("outer.rsp"), "a.cs @gone.rsp\n").unwrap();
        assert_eq!(
            expand(&strings(&["@outer.rsp"]), &dir),
            Err(cs2011(dir.join("gone.rsp")))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_at_with_no_file_name_is_cscs_cs2021() {
        let dir = scratch("bare-at");
        assert_eq!(
            expand(&strings(&["@"]), &dir),
            Err(String::from(
                "CS2021: File name '' is empty, contains invalid characters, has a drive specification without an absolute path, or is too long"
            ))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_response_file_that_includes_itself_is_refused_rather_than_followed() {
        let dir = scratch("cycle");
        fs::write(dir.join("self.rsp"), "@self.rsp\n").unwrap();
        let message = expand(&strings(&["@self.rsp"]), &dir).unwrap_err();
        assert!(message.contains("includes itself"), "{message}");
        fs::write(dir.join("a.rsp"), "x.cs @b.rsp\n").unwrap();
        fs::write(dir.join("b.rsp"), "@a.rsp\n").unwrap();
        let message = expand(&strings(&["@a.rsp"]), &dir).unwrap_err();
        assert!(message.contains("includes itself"), "{message}");
        assert!(
            message.contains("b.rsp"),
            "the message names the file the cycle runs through: {message}"
        );
        fs::write(dir.join("twice.rsp"), "a.cs\n").unwrap();
        assert_eq!(
            expand(&strings(&["@twice.rsp", "@twice.rsp"]), &dir).unwrap(),
            strings(&["a.cs", "a.cs"])
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_option_after_a_response_file_overrides_one_inside_it_and_the_other_way_round() {
        let dir = scratch("order");
        fs::write(dir.join("on.rsp"), "/unsafe+\n").unwrap();
        fs::write(dir.join("off.rsp"), "/unsafe-\n").unwrap();
        let unsafe_code = |args: &[&str]| {
            let mut args = strings(args);
            args.push(String::from("u.cs"));
            crate::parse_args(&expand(&args, &dir).unwrap())
                .unwrap()
                .lex
                .unsafe_code
        };
        assert!(!unsafe_code(&["@on.rsp", "/unsafe-"]));
        assert!(!unsafe_code(&["/unsafe+", "@off.rsp"]));
        assert!(unsafe_code(&["@off.rsp", "/unsafe+"]));
        let _ = fs::remove_dir_all(&dir);
    }
}
