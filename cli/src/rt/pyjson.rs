//! JSON text exactly as Python's `json.dumps` writes it, for every record and message the Python modules build
//! with it: insertion-ordered objects, `ensure_ascii` escapes, Python's separators and indentation, and Python's
//! float repr (shortest round trip; exponent form below 1e-4 and from 1e16). Values are `serde_json::Value`
//! built with the `preserve_order` and `arbitrary_precision` features, so key order and big integers survive.

use serde_json::{Map, Number, Value};
use std::fmt::Write;

#[derive(Clone, Copy, Debug)]
pub struct Opts<'a> {
    /// `indent=`: None writes one line; Some(n) writes n spaces per level (0 still breaks lines, as in Python).
    pub indent: Option<usize>,
    /// `separators=`: None means Python's default, (', ', ': ') on one line and (',', ': ') when indented.
    pub separators: Option<(&'a str, &'a str)>,
    pub ensure_ascii: bool,
    pub sort_keys: bool,
}

impl Default for Opts<'_> {
    fn default() -> Self {
        Opts { indent: None, separators: None, ensure_ascii: true, sort_keys: false }
    }
}

/// `json.dumps(value)` with Python's defaults.
pub fn dumps(value: &Value) -> String {
    dumps_with(value, Opts::default())
}

/// `json.dumps(value, indent=n)`.
pub fn dumps_indent(value: &Value, indent: usize) -> String {
    dumps_with(value, Opts { indent: Some(indent), ..Opts::default() })
}

/// `json.dumps(value, separators=(',', ':'))`, the compact form.
pub fn dumps_compact(value: &Value) -> String {
    dumps_with(value, Opts { separators: Some((",", ":")), ..Opts::default() })
}

pub fn dumps_with(value: &Value, opts: Opts) -> String {
    let (item, key) = opts.separators.unwrap_or(if opts.indent.is_some() { (",", ": ") } else { (", ", ": ") });
    let mut out = String::new();
    write_value(&mut out, value, &opts, item, key, 0);
    out
}

fn newline(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(n) = indent {
        out.push('\n');
        for _ in 0..n * level {
            out.push(' ');
        }
    }
}

fn write_value(out: &mut String, v: &Value, o: &Opts, item: &str, key: &str, level: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number(n)),
        Value::String(s) => string(out, s, o.ensure_ascii),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(item);
                }
                newline(out, o.indent, level + 1);
                write_value(out, x, o, item, key, level + 1);
            }
            newline(out, o.indent, level);
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            let mut entries: Vec<(&String, &Value)> = m.iter().collect();
            if o.sort_keys {
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            for (i, (k, x)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(item);
                }
                newline(out, o.indent, level + 1);
                string(out, k, o.ensure_ascii);
                out.push_str(key);
                write_value(out, x, o, item, key, level + 1);
            }
            newline(out, o.indent, level);
            out.push('}');
        }
    }
}

/// A JSON string literal as Python writes it.
pub fn string(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (ensure_ascii && (c as u32) > 0x7e) => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{:04x}", unit);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `json.dumps(str)` alone.
pub fn quote(s: &str) -> String {
    let mut out = String::new();
    string(&mut out, s, true);
    out
}

fn number(n: &Number) -> String {
    let text = n.to_string();
    if text.contains(['.', 'e', 'E']) {
        return float(text.parse::<f64>().unwrap_or(f64::NAN));
    }
    if text == "-0" { "0".into() } else { text }
}

/// Python's `float.__repr__` (and so json.dumps of a float, with NaN and Infinity as json writes them).
pub fn float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sci = format!("{f:e}"); // shortest round-trip digits, as Python's repr
    let (mantissa, exp) = sci.split_once('e').expect("LowerExp has an exponent");
    let exp: i32 = exp.parse().expect("an integer exponent");
    let (sign, mantissa) = mantissa.strip_prefix('-').map_or(("", mantissa), |m| ("-", m));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let decpt = exp + 1;
    let body = if !(-3..=16).contains(&decpt) {
        let rest = if digits.len() > 1 { format!(".{}", &digits[1..]) } else { String::new() };
        format!("{}{}e{}{:02}", &digits[..1], rest, if exp < 0 { '-' } else { '+' }, exp.unsigned_abs())
    } else if decpt <= 0 {
        format!("0.{}{}", "0".repeat((-decpt) as usize), digits)
    } else if decpt as usize >= digits.len() {
        format!("{}{}.0", digits, "0".repeat(decpt as usize - digits.len()))
    } else {
        format!("{}.{}", &digits[..decpt as usize], &digits[decpt as usize..])
    };
    format!("{sign}{body}")
}

/// A float as a JSON value that dumps as Python writes it (NaN and infinities included, as Python allows).
pub fn float_value(f: f64) -> Value {
    match Number::from_f64(f) {
        Some(n) => Value::Number(n),
        None => Value::Number(float(f).parse::<Number>().unwrap_or_else(|_| Number::from(0))),
    }
}

/// `json.loads(text)` for text Ringo itself wrote or a model replied with; the error is a ValueError.
pub fn loads(text: &str) -> crate::rt::Result<Value> {
    serde_json::from_str(text).map_err(|e| crate::rt::Error::value(e.to_string()))
}

/// An empty insertion-ordered object.
pub fn object() -> Map<String, Value> {
    Map::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_print_as_python_repr() {
        for (f, want) in [
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (1e-5, "1e-05"),
            (123.456, "123.456"),
            (-2.5e-7, "-2.5e-07"),
            (1e100, "1e+100"),
            (0.1 + 0.2, "0.30000000000000004"),
            (3.0, "3.0"),
            (-0.0, "-0.0"),
            (1.5e300, "1.5e+300"),
            (12345678901234567.0, "1.2345678901234568e+16"),
        ] {
            assert_eq!(float(f), want, "{f}");
        }
    }

    #[test]
    fn dumps_matches_python_layout() {
        let v = json!({"b": [1, 2.0, "é\u{1f600}\n"], "a": {}, "c": [], "d": null, "e": true});
        let want = r#"{"b": [1, 2.0, "~u00e9~ud83d~ude00~n"], "a": {}, "c": [], "d": null, "e": true}"#.replace('~', "\\");
        assert_eq!(dumps(&v), want);
        assert_eq!(dumps_indent(&json!({"a": [1, {"b": 2}]}), 1), "{\n \"a\": [\n  1,\n  {\n   \"b\": 2\n  }\n ]\n}");
        assert_eq!(dumps_compact(&json!({"a": [1, 2]})), r#"{"a":[1,2]}"#);
        let parsed = loads(r#"{"z": 1, "a": 2, "z": 3, "big": 123456789012345678901234567890}"#).unwrap();
        assert_eq!(dumps(&parsed), r#"{"z": 3, "a": 2, "big": 123456789012345678901234567890}"#);
        assert_eq!(dumps_with(&json!("\u{7f}é"), Opts { ensure_ascii: false, ..Opts::default() }), "\"\u{7f}é\"");
    }
}
