//! The one JSON escaper.
//!
//! Access logs, OTLP span payloads and the admin status document all serialise
//! strings that a client can influence — a request path, a `Host`, an upstream
//! address read back from config. Each of those grew its own escaping once,
//! and three copies of an escaper is three chances for one of them to be
//! wrong. There is one here instead, and everything calls it.
//!
//! Hand-written rather than `serde_json`: the documents are small and fixed in
//! shape, and a proxy's supply chain is worth more than the convenience.

use std::fmt::Write as _;

/// Escape `value` into `out` per RFC 8259.
///
/// A path is attacker-controlled: `/a"b` would otherwise close the string and
/// produce a line that breaks every downstream JSON parser, or worse, let a
/// crafted request forge extra fields.
pub fn escape_into(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// `"key":"value",` — the trailing comma included, because every caller that
/// uses this writes a run of fields and closes the object itself.
pub fn field_str(out: &mut String, key: &str, value: &str) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":\"");
    escape_into(out, value);
    out.push_str("\",");
}

/// A quoted, escaped string on its own — for values inside arrays.
pub fn quoted(out: &mut String, value: &str) {
    out.push('"');
    escape_into(out, value);
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_cannot_close_the_string() {
        let mut s = String::new();
        escape_into(&mut s, r#"a"b"#);
        assert_eq!(s, r#"a\"b"#);
    }

    #[test]
    fn control_characters_become_unicode_escapes() {
        let mut s = String::new();
        escape_into(&mut s, "a\u{0}b\u{1f}");
        assert_eq!(s, "a\\u0000b\\u001f");
    }

    #[test]
    fn a_newline_cannot_split_a_line() {
        let mut s = String::new();
        escape_into(&mut s, "a\nb");
        assert!(!s.contains('\n'));
    }
}
