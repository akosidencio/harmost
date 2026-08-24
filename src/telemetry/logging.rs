//! Structured access logs.
//!
//! # What is deliberately absent
//!
//! No `Authorization`, no `Cookie`, no request or response bodies, and no
//! query string. The query string is the subtle one: it routinely carries
//! session tokens, password-reset codes and signed URLs, and an access log is
//! the single most-copied artifact in an incident. The path alone is logged.

use std::fmt::Write as _;

/// One access log line.
pub struct AccessLog<'a> {
    pub method: &'a str,
    /// Path only. Never the query string.
    pub path: &'a str,
    pub route: &'a str,
    pub class: &'a str,
    pub cache: &'a str,
    pub upstream: Option<&'a str>,
    pub status: u16,
    pub shed: bool,
    pub origin_ms: u128,
    pub total_ms: u128,
}

impl AccessLog<'_> {
    pub fn to_json(&self) -> String {
        let mut s = String::with_capacity(224);
        s.push('{');
        write_str(&mut s, "method", self.method);
        write_str(&mut s, "path", self.path);
        write_str(&mut s, "route", self.route);
        write_str(&mut s, "class", self.class);
        write_str(&mut s, "cache", self.cache);
        write_str(&mut s, "upstream", self.upstream.unwrap_or("-"));
        let _ = write!(
            s,
            "\"status\":{},\"shed\":{},\"origin_ms\":{},\"total_ms\":{}}}",
            self.status, self.shed, self.origin_ms, self.total_ms
        );
        s
    }
}

fn write_str(out: &mut String, key: &str, value: &str) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":\"");
    escape_into(out, value);
    out.push_str("\",");
}

/// Escape per RFC 8259.
///
/// A path is attacker-controlled: `/a"b` would otherwise close the string and
/// produce a line that breaks every downstream JSON parser, or worse, lets a
/// crafted request forge extra fields in the log.
fn escape_into(out: &mut String, value: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn log(path: &str) -> String {
        AccessLog {
            method: "GET",
            path,
            route: "products",
            class: "public_document",
            cache: "hit",
            upstream: Some("next-1:3000"),
            status: 200,
            shed: false,
            origin_ms: 0,
            total_ms: 3,
        }
        .to_json()
    }

    #[test]
    fn renders_a_plain_line() {
        let out = log("/products/iphone");
        assert!(out.starts_with('{') && out.ends_with('}'));
        assert!(out.contains(r#""path":"/products/iphone""#));
        assert!(out.contains(r#""status":200"#));
    }

    #[test]
    fn a_quote_in_the_path_cannot_break_the_line() {
        let out = log(r#"/a"b"#);
        assert!(out.contains(r#""path":"/a\"b""#), "{out}");
        assert_eq!(out.matches(r#"","#).count(), 6, "field count changed: {out}");
    }

    #[test]
    fn a_crafted_path_cannot_forge_fields() {
        // Without escaping this would inject a `status` field of its own.
        let out = log(r#"/x","status":999,"x":"#);
        assert!(out.contains(r#""status":200"#));
        assert!(!out.contains(r#""status":999"#), "{out}");
    }

    #[test]
    fn control_characters_are_escaped() {
        let out = log("/a\nb\tc");
        assert!(out.contains("\\n"), "{out}");
        assert!(out.contains("\\t"), "{out}");
        assert!(!out.contains('\n'), "a raw newline would split the log line");
    }

    #[test]
    fn null_byte_is_escaped_as_a_unicode_sequence() {
        let out = log("/a\u{0}b");
        assert!(out.contains("\\u0000"), "{out}");
    }

    #[test]
    fn missing_upstream_renders_as_a_dash() {
        let out = AccessLog {
            method: "GET",
            path: "/x",
            route: "-",
            class: "public_document",
            cache: "hit",
            upstream: None,
            status: 200,
            shed: false,
            origin_ms: 0,
            total_ms: 1,
        }
        .to_json();
        assert!(out.contains(r#""upstream":"-""#));
    }
}
