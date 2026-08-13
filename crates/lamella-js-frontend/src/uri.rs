//! `encodeURI`, `encodeURIComponent`, `decodeURI` and `decodeURIComponent`.

use crate::builtins::arg;
use crate::interpreter::{Completion, Interpreter};
use crate::string_value::JsString;
use crate::value::JsValue;
use crate::format;

/// The characters `Encode` never escapes and `Decode` always unescapes, in EVERY variant: ASCII
/// word characters plus the "mark" punctuation. Everything else depends on which pair is in use.
const UNRESERVED_MARKS: &[u8] = b"-_.!~*'()";

/// What `encodeURI`/`decodeURI` add to that set: the punctuation that carries a URI's STRUCTURE.
///
/// This one list serves both directions on purpose. `encodeURI` leaving `/` alone and `decodeURI`
/// turning `%2F` into `/` would each look correct alone and together would lose the distinction
/// between a separator and a slash that was data.
const URI_RESERVED: &[u8] = b";/?:@&=+$,#";

pub(crate) fn install(interpreter: &mut Interpreter) {
    let encode_uri = interpreter.native_function("encodeURI", 1, |interpreter, _this, arguments| {
        with_string(interpreter, arguments, |interpreter, text| {
            encode(interpreter, text, URI_RESERVED)
        })
    });
    interpreter.define_global("encodeURI", JsValue::Object(encode_uri));

    let encode_component =
        interpreter.native_function("encodeURIComponent", 1, |interpreter, _this, arguments| {
            with_string(interpreter, arguments, |interpreter, text| {
                encode(interpreter, text, b"")
            })
        });
    interpreter.define_global("encodeURIComponent", JsValue::Object(encode_component));

    let decode_uri = interpreter.native_function("decodeURI", 1, |interpreter, _this, arguments| {
        with_string(interpreter, arguments, |interpreter, text| {
            decode(interpreter, text, URI_RESERVED)
        })
    });
    interpreter.define_global("decodeURI", JsValue::Object(decode_uri));

    let decode_component =
        interpreter.native_function("decodeURIComponent", 1, |interpreter, _this, arguments| {
            with_string(interpreter, arguments, |interpreter, text| {
                decode(interpreter, text, b"")
            })
        });
    interpreter.define_global("decodeURIComponent", JsValue::Object(decode_component));
}

/// `ToString(argument)` and then the work, so a throwing `toString` propagates rather than being
/// swallowed into a `URIError` about input the program never supplied.
fn with_string(
    interpreter: &mut Interpreter,
    arguments: &[JsValue],
    body: impl FnOnce(&mut Interpreter, &JsString) -> Completion,
) -> Completion {
    match interpreter.to_string_value(&arg(arguments, 0)) {
        Ok(text) => body(interpreter, &text),
        Err(abrupt) => abrupt,
    }
}

/// The standard's `Encode`, with `extra_unescaped` naming what this variant preserves beyond the
/// unreserved set.
fn encode(interpreter: &mut Interpreter, text: &JsString, extra_unescaped: &[u8]) -> Completion {
    let units = text.units();
    let mut out = JsString::new();
    let mut index = 0usize;
    while index < units.len() {
        let unit = units[index];
        if is_preserved(unit, extra_unescaped) {
            out.push_code_unit(unit);
            index += 1;
            continue;
        }
        let (code_point, consumed) = match code_point_at(units, index) {
            Some(pair) => pair,
            None => {
                return interpreter.throw(
                    "URIError",
                    "a lone surrogate is not a character and cannot be percent-encoded",
                )
            }
        };
        let mut buffer = [0u8; 4];
        for octet in code_point.encode_utf8(&mut buffer).as_bytes() {
            out.push_str(&format!("%{octet:02X}"));
        }
        index += consumed;
    }
    Completion::Normal(JsValue::String(out))
}

/// The standard's `Decode`, with `preserve_escape_set` naming the characters whose escape is COPIED
/// THROUGH rather than resolved.
fn decode(interpreter: &mut Interpreter, text: &JsString, preserve_escape_set: &[u8]) -> Completion {
    let units = text.units();
    let mut out = JsString::new();
    let mut index = 0usize;
    while index < units.len() {
        if units[index] != u16::from(b'%') {
            out.push_code_unit(units[index]);
            index += 1;
            continue;
        }
        let Some(first) = hex_octet(units, index + 1) else {
            return malformed(interpreter);
        };
        let length = first.leading_ones() as usize;
        if length == 0 {
            if preserve_escape_set.contains(&first) {
                out.push_code_unit(units[index]);
                out.push_code_unit(units[index + 1]);
                out.push_code_unit(units[index + 2]);
            } else {
                out.push_code_unit(u16::from(first));
            }
            index += 3;
            continue;
        }
        if !(2..=4).contains(&length) {
            return malformed(interpreter);
        }
        let mut octets = [0u8; 4];
        octets[0] = first;
        let mut cursor = index + 3;
        for slot in octets.iter_mut().take(length).skip(1) {
            if units.get(cursor) != Some(&u16::from(b'%')) {
                return malformed(interpreter);
            }
            let Some(octet) = hex_octet(units, cursor + 1) else {
                return malformed(interpreter);
            };
            *slot = octet;
            cursor += 3;
        }
        let Some(code_point) = decode_utf8(&octets[..length]) else {
            return malformed(interpreter);
        };
        out.push_char(code_point);
        index = cursor;
    }
    Completion::Normal(JsValue::String(out))
}

fn malformed(interpreter: &mut Interpreter) -> Completion {
    interpreter.throw("URIError", "the URI contains a malformed escape sequence")
}

/// Whether a code unit is left alone by this variant of `Encode`.
///
/// Above `U+007F` the answer is always false, and the comparison is on the CODE UNIT rather than
/// on a decoded character: no non-ASCII character is ever preserved, so the surrogate check below
/// is reached for every one of them.
fn is_preserved(unit: u16, extra_unescaped: &[u8]) -> bool {
    let Ok(byte) = u8::try_from(unit) else { return false };
    byte.is_ascii_alphanumeric()
        || UNRESERVED_MARKS.contains(&byte)
        || extra_unescaped.contains(&byte)
}

/// The standard's `ParseHexOctet`: exactly two hex digits at `position`, or nothing.
///
/// `char::to_digit` would accept the non-ASCII digits Unicode also calls hexadecimal. The
/// standard's grammar is `HexDigit`, which is ASCII only.
fn hex_octet(units: &[u16], position: usize) -> Option<u8> {
    let high = hex_digit(*units.get(position)?)?;
    let low = hex_digit(*units.get(position + 1)?)?;
    Some(high << 4 | low)
}

fn hex_digit(unit: u16) -> Option<u8> {
    let byte = u8::try_from(unit).ok()?;
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// `CodePointAt`: the code point beginning at `index` and how many units it occupies, or `None`
/// when the unit there is an UNPAIRED surrogate.
fn code_point_at(units: &[u16], index: usize) -> Option<(char, usize)> {
    let unit = units[index];
    if !(0xD800..=0xDFFF).contains(&unit) {
        return char::from_u32(u32::from(unit)).map(|ch| (ch, 1));
    }
    if unit >= 0xDC00 {
        return None;
    }
    let trailing = *units.get(index + 1)?;
    if !(0xDC00..=0xDFFF).contains(&trailing) {
        return None;
    }
    let code_point =
        0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(trailing) - 0xDC00);
    char::from_u32(code_point).map(|ch| (ch, 2))
}

/// Decodes `octets` as ONE UTF-8 sequence, refusing everything UTF-8 refuses.
///
/// THE THREE REFUSALS ARE THE POINT AND ARE EASY TO OMIT, because reassembling the payload bits
/// succeeds without any of them: a continuation byte that is not `10xxxxxx`, an OVERLONG form that
/// spells a small value in more bytes than it needs, and a result that is a surrogate or above
/// `U+10FFFF`. `char::from_u32` catches only the last of the three, which is why the length check
/// is written out rather than left to it.
fn decode_utf8(octets: &[u8]) -> Option<char> {
    let mut value = u32::from(octets[0] & (0x7F >> octets.len()));
    for octet in &octets[1..] {
        if octet & 0xC0 != 0x80 {
            return None;
        }
        value = value << 6 | u32::from(octet & 0x3F);
    }
    let shortest = match octets.len() {
        2 => 0x80,
        3 => 0x800,
        _ => 0x1_0000,
    };
    if value < shortest {
        return None;
    }
    char::from_u32(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::String;

    fn run(interpreter: &mut Interpreter, source: &str) -> Result<String, String> {
        match interpreter.eval_source(source) {
            Ok(JsValue::String(text)) => Ok(text.to_lossy_string()),
            Ok(other) => Err(format!("not a string: {other:?}")),
            Err(error) => Err(format!("{error}")),
        }
    }

    #[test]
    fn the_component_form_escapes_the_punctuation_the_uri_form_keeps() {
        let mut interpreter = Interpreter::new();
        assert_eq!(run(&mut interpreter, "encodeURI('a/b?c=d#e')").unwrap(), "a/b?c=d#e");
        assert_eq!(
            run(&mut interpreter, "encodeURIComponent('a/b?c=d#e')").unwrap(),
            "a%2Fb%3Fc%3Dd%23e"
        );
    }

    /// THE ROUND TRIP IS THE PROPERTY, and it only holds because the preserved set is the same in
    /// both directions. `decodeURI` unescaping `%2F` would break it in exactly one character.
    #[test]
    fn an_escaped_separator_survives_the_uri_round_trip() {
        let mut interpreter = Interpreter::new();
        assert_eq!(run(&mut interpreter, "decodeURI('a%2Fb')").unwrap(), "a%2Fb");
        assert_eq!(run(&mut interpreter, "decodeURIComponent('a%2Fb')").unwrap(), "a/b");
        assert_eq!(run(&mut interpreter, "decodeURI(encodeURI('a%2Fb'))").unwrap(), "a%2Fb");
    }

    #[test]
    fn a_character_outside_ascii_becomes_its_utf8_octets_in_uppercase_hex() {
        let mut interpreter = Interpreter::new();
        assert_eq!(run(&mut interpreter, "encodeURIComponent('\u{e4}')").unwrap(), "%C3%A4");
        assert_eq!(run(&mut interpreter, "encodeURIComponent('\u{1F600}')").unwrap(), "%F0%9F%98%80");
        assert_eq!(run(&mut interpreter, "decodeURIComponent('%F0%9F%98%80')").unwrap(), "\u{1F600}");
    }

    /// A LONE SURROGATE IS ORDINARY INPUT and the one case an "encode to UTF-8 and escape the
    /// bytes" implementation gets wrong while looking successful.
    #[test]
    fn a_lone_surrogate_is_a_uri_error_in_either_half_of_the_pair() {
        let mut interpreter = Interpreter::new();
        for source in ["encodeURIComponent('\\uD800')", "encodeURIComponent('\\uDC00')"] {
            assert!(run(&mut interpreter, source).unwrap_err().contains("URIError"), "{source}");
        }
        assert_eq!(run(&mut interpreter, "encodeURIComponent('\\uD83D\\uDE00')").unwrap(), "%F0%9F%98%80");
    }

    /// EACH OF THESE REASSEMBLES INTO A PLAUSIBLE NUMBER, which is why a decoder that only
    /// gathers the payload bits accepts all of them.
    #[test]
    fn decoding_refuses_every_shape_utf8_refuses() {
        let mut interpreter = Interpreter::new();
        for source in [
            "decodeURIComponent('%80')",
            "decodeURIComponent('%C0%80')",
            "decodeURIComponent('%E0%80%80')",
            "decodeURIComponent('%ED%A0%80')",
            "decodeURIComponent('%F5%80%80%80')",
            "decodeURIComponent('%F8%88%80%80%80')",
            "decodeURIComponent('%C3')",
            "decodeURIComponent('%C3%28')",
            "decodeURIComponent('%2')",
            "decodeURIComponent('%zz')",
        ] {
            let outcome = run(&mut interpreter, source);
            assert!(
                outcome.as_ref().is_err_and(|error| error.contains("URIError")),
                "{source} should be a URIError, was {outcome:?}"
            );
        }
    }

    #[test]
    fn the_unreserved_set_is_never_escaped() {
        let mut interpreter = Interpreter::new();
        let unreserved = "ABCXYZabcxyz0189-_.!~*'()";
        assert_eq!(
            run(&mut interpreter, &format!("encodeURIComponent(\"{unreserved}\")")).unwrap(),
            unreserved
        );
    }
}
