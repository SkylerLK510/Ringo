//! Python semantics the ported modules rely on, where Rust's std differs: `str.isspace` (it also counts
//! U+001C..U+001F), `str.strip()`, `str.split()`, `str.splitlines()`, code-point lengths and slices, `repr(str)`,
//! `str.upper()`, re's `\w` and `\d`, `int()`, `float()` and `round()`, `bytes.decode('utf-8')`, textwrap, the
//! text of an `OSError`, the Python objects json.loads makes of JSON values (`type(x).__name__`, truth, `str()`,
//! `repr()`, `==`, and `Num`, an int or a float), and POSIX paths and files as pathlib, os.path and tempfile
//! handle them.

use crate::rt::{Error, Exc, Result};
use serde_json::{Number, Value};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Python's `str.isspace()` for one character: Unicode White_Space plus the four information separators.
pub fn isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub fn strip(s: &str) -> &str {
    s.trim_matches(isspace)
}

pub fn lstrip(s: &str) -> &str {
    s.trim_start_matches(isspace)
}

pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(isspace)
}

/// `str.split()` with no arguments: runs of whitespace separate, and none are empty.
pub fn split(s: &str) -> Vec<&str> {
    s.split(isspace).filter(|p| !p.is_empty()).collect()
}

fn line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `str.splitlines(keepends)`: every Python line boundary, `\r\n` as one.
pub fn splitlines(s: &str, keepends: bool) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if !line_break(c) {
            continue;
        }
        let mut end = i + c.len_utf8();
        if c == '\r'
            && let Some(&(j, '\n')) = it.peek() {
                it.next();
                end = j + 1;
            }
        out.push(if keepends { &s[start..end] } else { &s[start..i] });
        start = end;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `len(s)`: code points, not bytes.
pub fn len(s: &str) -> usize {
    s.chars().count()
}

fn byte_at(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(i, _)| i)
}

/// `s[:n]` by code points.
pub fn head(s: &str, n: usize) -> &str {
    &s[..byte_at(s, n)]
}

/// `s[-n:]` by code points (`s[-0:]` is the whole string in Python, and so it is here).
pub fn tail(s: &str, n: usize) -> &str {
    let count = len(s);
    if n == 0 || n >= count {
        return s;
    }
    &s[byte_at(s, count - n)..]
}

/// `s[a:b]` by code points, for non-negative bounds.
pub fn slice(s: &str, a: usize, b: usize) -> &str {
    let start = byte_at(s, a);
    let end = byte_at(s, b.max(a));
    &s[start..end]
}

/// Python's `repr(str)` exactly: its quote choice, escapes, and `str.isprintable()` from Python's Unicode data.
pub fn repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        let u = c as u32;
        match c {
            _ if c == quote || c == '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ if u < 0x20 || u == 0x7f => out.push_str(&format!("\\x{u:02x}")),
            _ if u < 0x7f || isprintable(c) => out.push(c),
            _ if u <= 0xff => out.push_str(&format!("\\x{u:02x}")),
            _ if u <= 0xffff => out.push_str(&format!("\\u{u:04x}")),
            _ => out.push_str(&format!("\\U{u:08x}")),
        }
    }
    out.push(quote);
    out
}

// ---------------------------------------------------------------------------------------------------------------
// Unicode classes as Python's own data has them (the tables at the end of this file)

/// re's `\w` (`str.isalnum()` or `_`) as the body of a regex character class: Unicode letters, numbers and `_`.
pub const WORD: &str = r"\p{L}\p{N}_";
/// re's `\s` (`str.isspace()`) as the body of a regex character class: it adds U+001C..U+001F to Unicode's `\s`.
pub const SPACE: &str = r"\s\x1c-\x1f";

fn table(src: &str) -> Vec<(u32, u32)> {
    src.split_ascii_whitespace()
        .map(|r| {
            let (a, b) = r.split_once('-').unwrap_or((r, r));
            (u32::from_str_radix(a, 16).expect("hex"), u32::from_str_radix(b, 16).expect("hex"))
        })
        .collect()
}

fn find(t: &[(u32, u32)], c: char) -> Option<(u32, u32)> {
    let c = c as u32;
    t.binary_search_by(|&(a, b)| {
        if b < c {
            Ordering::Less
        } else if a > c {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    })
    .ok()
    .map(|i| t[i])
}

static WORD_T: LazyLock<Vec<(u32, u32)>> = LazyLock::new(|| table(WORD_TABLE));
static DECIMAL_T: LazyLock<Vec<(u32, u32)>> = LazyLock::new(|| table(DECIMAL_TABLE));
static UNPRINTABLE_T: LazyLock<Vec<(u32, u32)>> = LazyLock::new(|| table(UNPRINTABLE_TABLE));
static UPPER_T: LazyLock<Vec<(u32, u32)>> = LazyLock::new(|| table(UPPER_TABLE));

/// `str.upper()`: Rust's full case mapping, applied only to the code points Python's Unicode data maps (std may
/// follow a newer Unicode version, which cases letters Python leaves as they are).
pub fn upper(s: &str) -> String {
    s.chars().flat_map(|c| if find(&UPPER_T, c).is_some() { c.to_uppercase().collect::<Vec<_>>() } else { vec![c] }).collect()
}

/// re's `\w` for str patterns: `str.isalnum()` or `_`.
pub fn is_word(c: char) -> bool {
    find(&WORD_T, c).is_some()
}

/// re's `\d` for str patterns (`str.isdecimal()`) with the digit's value.
pub fn decimal_digit(c: char) -> Option<u32> {
    // Unicode keeps every decimal digit in runs of ten from 0 to 9; the table merges adjacent runs.
    find(&DECIMAL_T, c).map(|(a, _)| (c as u32 - a) % 10)
}

/// `str.isprintable()` for one character.
pub fn isprintable(c: char) -> bool {
    find(&UNPRINTABLE_T, c).is_none()
}

// ---------------------------------------------------------------------------------------------------------------
// Python objects from JSON values

/// Whether a JSON number's text is a float (json.loads makes an int of the others).
pub fn is_float_text(t: &str) -> bool {
    t.contains(['.', 'e', 'E'])
}

/// A JSON int's value (not a float or bool), when it fits in 128 bits.
pub fn int_of(v: &Value) -> Option<i128> {
    match v {
        Value::Number(n) if !is_float_text(n.as_str()) => n.as_str().parse().ok(),
        _ => None,
    }
}

/// `type(x).__name__` of the object json.loads makes of a value.
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if is_float_text(n.as_str()) => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `bool(x)`.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => {
            let t = n.as_str();
            if is_float_text(t) { t.parse::<f64>() != Ok(0.0) } else { t.bytes().any(|b| b.is_ascii_digit() && b != b'0') }
        }
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// `repr(float)`.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        "nan".into()
    } else if f.is_infinite() {
        if f > 0.0 { "inf".into() } else { "-inf".into() }
    } else {
        crate::rt::pyjson::float(f)
    }
}

/// `repr(x)` of the object json.loads makes of a value, without recursion (values may nest deeply).
pub fn value_repr(v: &Value) -> String {
    enum T<'a> {
        V(&'a Value),
        S(&'static str),
        K(&'a str),
    }
    let mut out = String::new();
    let mut stack = vec![T::V(v)];
    while let Some(t) = stack.pop() {
        match t {
            T::S(s) => out.push_str(s),
            T::K(k) => {
                out.push_str(&repr(k));
                out.push_str(": ");
            }
            T::V(Value::Array(items)) => {
                stack.push(T::S("]"));
                for (i, x) in items.iter().enumerate().rev() {
                    stack.push(T::V(x));
                    if i > 0 {
                        stack.push(T::S(", "));
                    }
                }
                stack.push(T::S("["));
            }
            T::V(Value::Object(map)) => {
                stack.push(T::S("}"));
                for (i, (k, x)) in map.iter().enumerate().collect::<Vec<_>>().into_iter().rev() {
                    stack.push(T::V(x));
                    stack.push(T::K(k));
                    if i > 0 {
                        stack.push(T::S(", "));
                    }
                }
                stack.push(T::S("{"));
            }
            T::V(Value::Null) => out.push_str("None"),
            T::V(Value::Bool(b)) => out.push_str(if *b { "True" } else { "False" }),
            T::V(Value::Number(n)) => match n.as_str() {
                t if is_float_text(t) => out.push_str(&float_repr(t.parse().unwrap_or(f64::NAN))),
                "-0" => out.push('0'),
                t => out.push_str(t),
            },
            T::V(Value::String(s)) => out.push_str(&repr(s)),
        }
    }
    out
}

/// `str(x)` of the object json.loads makes of a value, as an f-string writes it.
pub fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => value_repr(other),
    }
}

/// `a == b` for JSON values (numbers compare by value across int, float and bool), without recursion.
pub fn eq(a: &Value, b: &Value) -> bool {
    let mut stack = vec![(a, b)];
    while let Some((a, b)) = stack.pop() {
        let same = match (a, b) {
            (Value::Null, Value::Null) => true,
            (Value::String(x), Value::String(y)) => x == y,
            (Value::Array(x), Value::Array(y)) => {
                stack.extend(x.iter().zip(y));
                x.len() == y.len()
            }
            (Value::Object(x), Value::Object(y)) => {
                x.len() == y.len()
                    && x.iter().all(|(k, v)| match y.get(k) {
                        Some(w) => {
                            stack.push((v, w));
                            true
                        }
                        None => false,
                    })
            }
            _ => num_eq(a, b),
        };
        if !same {
            return false;
        }
    }
    true
}

/// `a == b` for two numbers (a bool is 0 or 1): exact between an int and a float, for ints of any size.
fn num_eq(a: &Value, b: &Value) -> bool {
    // Err: an int past 128 bits, as its digits
    fn num(v: &Value) -> Option<std::result::Result<Num, &str>> {
        match v {
            Value::Bool(b) => Some(Ok(Num::Int(*b as i128))),
            Value::Number(n) => Some(Num::from_value(v).ok_or(n.as_str())),
            _ => None,
        }
    }
    match (num(a), num(b)) {
        (Some(Ok(x)), Some(Ok(y))) => x.py_cmp(y) == Some(Ordering::Equal),
        (Some(Err(x)), Some(Err(y))) => x == y,
        (Some(Err(big)), Some(Ok(Num::Float(f)))) | (Some(Ok(Num::Float(f))), Some(Err(big))) => {
            f.fract() == 0.0 && format!("{f:.0}") == big // an integral double prints exactly with {:.0}
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Numbers

/// A Python number: an int stays an int and a float a float, as Python keeps them. Python ints are unbounded;
/// these are 128-bit, and arithmetic past that is an OverflowError.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    Int(i128),
    Float(f64),
}

impl Num {
    /// The number json.loads makes of a JSON number (None for any other value, and for an int past 128 bits).
    pub fn from_value(v: &Value) -> Option<Num> {
        let Value::Number(n) = v else { return None };
        let s = n.as_str();
        if is_float_text(s) { s.parse().ok().map(Num::Float) } else { s.parse().ok().map(Num::Int) }
    }

    /// The JSON value json.dumps writes for it (an infinity as Infinity; NaN has no JSON value here: null).
    pub fn to_value(self) -> Value {
        match self {
            Num::Int(i) => Value::Number(Number::from_i128(i).expect("arbitrary_precision holds any i128")),
            Num::Float(f) => crate::rt::pyjson::float_value(f),
        }
    }

    pub fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }

    /// `str(x)`.
    pub fn py_str(self) -> String {
        match self {
            Num::Int(i) => i.to_string(),
            Num::Float(f) => float_repr(f),
        }
    }

    pub fn type_name(self) -> &'static str {
        match self {
            Num::Int(_) => "int",
            Num::Float(_) => "float",
        }
    }

    /// Python's comparison, exact between an int and a float (None when a NaN is involved).
    pub fn py_cmp(self, other: Num) -> Option<Ordering> {
        match (self, other) {
            (Num::Int(a), Num::Int(b)) => Some(a.cmp(&b)),
            (Num::Float(a), Num::Float(b)) => a.partial_cmp(&b),
            (Num::Int(a), Num::Float(b)) => cmp_int_float(a, b),
            (Num::Float(a), Num::Int(b)) => cmp_int_float(b, a).map(Ordering::reverse),
        }
    }

    pub fn gt(self, other: Num) -> bool {
        self.py_cmp(other) == Some(Ordering::Greater)
    }

    /// `max(self, other)`: other only when it is greater, as Python's max keeps the first of equals.
    pub fn max(self, other: Num) -> Num {
        if other.gt(self) { other } else { self }
    }

    /// `k * self` (an int past 128 bits saturates).
    pub fn times(self, k: i128) -> Num {
        match self {
            Num::Int(i) => Num::Int(i.saturating_mul(k)),
            Num::Float(f) => Num::Float(k as f64 * f),
        }
    }

    /// `self + other` for ints and floats.
    pub fn plus(self, other: Num) -> Result<Num> {
        match (self, other) {
            (Num::Int(a), Num::Int(b)) => a.checked_add(b).map(Num::Int).ok_or_else(too_big),
            (a, b) => Ok(Num::Float(a.as_f64() + b.as_f64())),
        }
    }

    /// `f'{x:.{prec}f}'` (an int is formatted as a float, as Python does; ties round to even).
    pub fn format_f(self, prec: usize) -> String {
        format!("{:.*}", prec, self.as_f64())
    }
}

fn cmp_int_float(i: i128, f: f64) -> Option<Ordering> {
    const TWO_127: f64 = 170141183460469231731687303715884105728.0;
    if f.is_nan() {
        return None;
    }
    if f >= TWO_127 {
        return Some(Ordering::Less);
    }
    if f < -TWO_127 {
        return Some(Ordering::Greater);
    }
    let t = f.trunc();
    Some(match i.cmp(&(t as i128)) {
        Ordering::Equal => (0.0).partial_cmp(&(f - t))?,
        o => o,
    })
}

/// The OverflowError of an int past 128 bits, where Python's would simply grow.
pub fn too_big() -> Error {
    Error::new(Exc::Overflow, "the native runtime keeps integers within 128 bits")
}

// ---------------------------------------------------------------------------------------------------------------
// Numbers from text

/// Python's `int(text)` for a str: whitespace, a sign, decimal digits of any script, single underscores between
/// digits, at most 4300 digits (int()'s limit); past i64 it saturates (Python's ints are unbounded).
pub fn int(text: &str) -> Option<i64> {
    let t = strip(text);
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let c: Vec<char> = t.chars().collect();
    if c.is_empty() || c[0] == '_' || c[c.len() - 1] == '_' {
        return None;
    }
    let (mut v, mut digits) = (0i128, 0);
    for (i, &ch) in c.iter().enumerate() {
        if ch == '_' {
            if c[i - 1] == '_' {
                return None;
            }
            continue;
        }
        v = v.saturating_mul(10).saturating_add(decimal_digit(ch)? as i128);
        digits += 1;
    }
    if digits > 4300 {
        return None;
    }
    Some((if neg { -v } else { v }).clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

/// Python's `float(text)` for a str, with its ValueError: Python's whitespace stripped, underscores only between
/// digits, the inf and nan words (ASCII digits only; Python also reads other Unicode decimal digits).
pub fn float(text: &str) -> Result<f64> {
    let bad = || Error::value(format!("could not convert string to float: {}", repr(text)));
    let t = strip(text);
    let chars: Vec<char> = t.chars().collect();
    let ok = chars.iter().enumerate().all(|(i, &c)| {
        c != '_' || (i > 0 && i + 1 < chars.len() && chars[i - 1].is_ascii_digit() && chars[i + 1].is_ascii_digit())
    });
    if !ok || !t.is_ascii() {
        return Err(bad());
    }
    t.chars().filter(|&c| c != '_').collect::<String>().parse().map_err(|_| bad())
}

/// `round(x, ndigits)` for a float: correctly rounded, ties to even.
pub fn round(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.ndigits$}").parse().expect("a formatted float parses")
}

// ---------------------------------------------------------------------------------------------------------------
// Bytes and errors

/// `bytes.decode('utf-8')`: strict, with the UnicodeDecodeError Python raises (the bytes and the reason).
pub fn decode_utf8(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|e| {
        let (b, start) = (e.as_bytes(), e.utf8_error().valid_up_to());
        let (end, reason) = match e.utf8_error().error_len() {
            None => (b.len(), "unexpected end of data"),
            Some(n) if matches!(b[start], 0x80..=0xc1 | 0xf5..=0xff) => (start + n, "invalid start byte"),
            Some(n) => (start + n, "invalid continuation byte"),
        };
        Error::new(
            Exc::UnicodeDecode,
            if end == start + 1 {
                format!("'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}", b[start])
            } else {
                format!("'utf-8' codec can't decode bytes in position {start}-{}: {reason}", end - 1)
            },
        )
    })
}

/// Text read in Python's universal-newlines mode: `\r\n` and `\r` become `\n`.
pub fn universal_newlines(text: String) -> String {
    if text.contains('\r') { text.replace("\r\n", "\n").replace('\r', "\n") } else { text }
}

// ---------------------------------------------------------------------------------------------------------------
// Paths and files, POSIX: pathlib, os.path, os.replace and tempfile, with Python's normalisation and error texts


/// A path's bytes.
#[cfg(unix)]
pub fn bytes_of(p: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes()
}

/// The path of some bytes.
#[cfg(unix)]
pub fn path_of(b: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(b))
}

#[cfg(not(unix))]
pub fn bytes_of(p: &Path) -> &[u8] {
    p.to_str().unwrap_or("").as_bytes()
}

#[cfg(not(unix))]
pub fn path_of(b: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&b).into_owned())
}

/// `str(PurePosixPath(p))`: one root ('/' or exactly '//'), no empty or '.' parts, '.' when nothing is left.
pub fn pure_path(p: &[u8]) -> Vec<u8> {
    let root: &[u8] = if p.starts_with(b"//") && !p.starts_with(b"///") {
        b"//"
    } else if p.starts_with(b"/") {
        b"/"
    } else {
        b""
    };
    let parts: Vec<&[u8]> = p.split(|&c| c == b'/').filter(|x| !x.is_empty() && *x != b".").collect();
    let mut out = root.to_vec();
    out.extend(parts.join(&b'/'));
    if out.is_empty() {
        out.push(b'.');
    }
    out
}

/// `Path(p)`, normalised as pathlib normalises it.
pub fn path(p: &Path) -> PathBuf {
    path_of(pure_path(bytes_of(p)))
}

/// `str(PurePosixPath(p))` of a str.
pub fn path_str(p: &str) -> String {
    String::from_utf8(pure_path(p.as_bytes())).expect("only ASCII separators and parts were dropped")
}

/// `Path(base) / name`.
pub fn join(base: &Path, name: &str) -> PathBuf {
    path(&base.join(name))
}

/// `PurePosixPath(p).parent` of a normalised path.
pub fn parent(p: &Path) -> PathBuf {
    let b = bytes_of(p);
    path_of(match b.iter().rposition(|&c| c == b'/') {
        None => b".".to_vec(),
        Some(0) => b"/".to_vec(),
        Some(1) if b.starts_with(b"//") => b"//".to_vec(),
        Some(i) => b[..i].to_vec(),
    })
}

/// `PurePosixPath(p).name`: the last part, '' for none.
pub fn path_name(p: &str) -> &str {
    p.split('/').rfind(|x| !x.is_empty() && *x != ".").unwrap_or("")
}

/// `os.path.normpath` (lexical: `..` removes the part before it).
pub fn normpath(p: &[u8]) -> Vec<u8> {
    let initial = if p.starts_with(b"//") && !p.starts_with(b"///") { 2 } else { usize::from(p.starts_with(b"/")) };
    let mut comps: Vec<&[u8]> = Vec::new();
    for comp in p.split(|&c| c == b'/') {
        if comp.is_empty() || comp == b"." {
            continue;
        }
        if comp != b".." || (initial == 0 && comps.is_empty()) || comps.last() == Some(&&b".."[..]) {
            comps.push(comp);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let mut out = vec![b'/'; initial];
    out.extend(comps.join(&b'/'));
    if out.is_empty() {
        out.push(b'.');
    }
    out
}

/// `os.path.abspath`.
pub fn abspath(p: &Path) -> Result<PathBuf> {
    let b = bytes_of(p);
    if b.starts_with(b"/") {
        return Ok(path_of(normpath(b)));
    }
    let mut full = bytes_of(&std::env::current_dir()?).to_vec();
    full.push(b'/');
    full.extend(b);
    Ok(path_of(normpath(&full)))
}

/// `os.path.realpath(path)` (strict=False; `Path.resolve()`), as Python 3.14 resolves it: absolute, symbolic links
/// resolved as far as they exist, `..` applied to the resolved path; a component that cannot be examined, and a
/// symlink loop, stay as they are. An embedded NUL is lstat's ValueError.
#[cfg(unix)]
pub fn realpath(p: &Path) -> Result<PathBuf> {
    let filename = bytes_of(p);
    let split = |b: &[u8]| -> Vec<Option<Vec<u8>>> { b.split(|c| *c == b'/').rev().map(|p| Some(p.to_vec())).collect() };
    let mut rest = split(filename);
    let mut part_count = rest.len();
    let mut path: Vec<u8> = if filename.starts_with(b"/") { b"/".to_vec() } else { bytes_of(&std::env::current_dir()?).to_vec() };
    let mut seen: std::collections::HashMap<Vec<u8>, Option<Vec<u8>>> = std::collections::HashMap::new();
    while part_count > 0 {
        let Some(entry) = rest.pop() else { break };
        let Some(name) = entry else {
            // a resolved symlink marker: the link below it now resolves to path
            if let Some(Some(link)) = rest.pop() {
                seen.insert(link, Some(path.clone()));
            }
            continue;
        };
        part_count -= 1;
        if name.is_empty() || name == b"." {
            continue;
        }
        if name == b".." {
            let cut = path.iter().rposition(|&b| b == b'/').unwrap_or(0);
            path.truncate(cut);
            if path.is_empty() {
                path.push(b'/');
            }
            continue;
        }
        let mut newpath = path.clone();
        if path != b"/" {
            newpath.push(b'/');
        }
        newpath.extend_from_slice(&name);
        if newpath.contains(&0) {
            return Err(Error::value("lstat: embedded null character in path"));
        }
        let as_path = path_of(newpath.clone());
        let is_link = std::fs::symlink_metadata(&as_path).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            path = newpath;
            continue;
        }
        if let Some(known) = seen.get(&newpath) {
            path = known.clone().unwrap_or(newpath); // None: a symlink loop, left unresolved
            continue;
        }
        let Ok(target) = std::fs::read_link(&as_path) else {
            path = newpath;
            continue;
        };
        let target = bytes_of(&target).to_vec();
        if target.starts_with(b"/") {
            path = b"/".to_vec();
        }
        seen.insert(newpath.clone(), None);
        rest.push(Some(newpath));
        rest.push(None);
        let parts = split(&target);
        part_count += parts.len();
        rest.extend(parts);
    }
    Ok(path_of(path))
}

#[cfg(not(unix))]
pub fn realpath(_: &Path) -> Result<PathBuf> {
    Err(Error::os(crate::rt::NATIVE_ONLY))
}

/// The OSError of a call on a path, naming it as `str(path)` does.
pub fn path_error(e: &std::io::Error, p: &Path) -> Error {
    Error::io(e, Some(&p.to_string_lossy()))
}

/// `Path.mkdir(mode, parents, exist_ok)`, with pathlib's recursion and errors.
#[cfg(unix)]
pub fn mkdir(p: &Path, mode: u32, parents: bool, exist_ok: bool) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let p = &path(p);
    match std::fs::DirBuilder::new().mode(mode).create(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let up = parent(p);
            if !parents || up == *p {
                return Err(path_error(&e, p));
            }
            mkdir(&up, 0o777, true, true)?;
            mkdir(p, mode, false, exist_ok)
        }
        Err(e) => {
            if !exist_ok || !p.is_dir() {
                return Err(path_error(&e, p));
            }
            Ok(())
        }
    }
}

#[cfg(not(unix))]
pub fn mkdir(_: &Path, _mode: u32, _parents: bool, _exist_ok: bool) -> Result<()> {
    Err(Error::os(crate::rt::NATIVE_ONLY))
}

/// `os.replace(src, dst)` (and `Path.rename`): its OSError names both paths ("...: 'a' -> 'b'").
pub fn replace(src: &Path, dst: &Path) -> Result<()> {
    std::fs::rename(src, dst).map_err(|e| {
        Error::os(format!("{}: {} -> {}", os_error(&e, None), repr(&src.to_string_lossy()), repr(&dst.to_string_lossy())))
    })
}

/// `Path.write_text` / `write_bytes`: created or truncated, 0o666 less the umask; only opening it names the path.
pub fn write_file(p: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(p).map_err(|e| path_error(&e, p))?;
    f.write_all(data).map_err(Error::from)
}

/// `Path(p).read_text(encoding='utf-8')`: strict UTF-8 (UnicodeDecodeError's text) and universal newlines. Opening
/// it names `str(path)`, a folder included (IsADirectoryError); a failed read does not.
pub fn read_text(p: &Path) -> Result<String> {
    use std::io::Read;
    let p = &path(p);
    let mut f = std::fs::File::open(p).map_err(|e| path_error(&e, p))?;
    #[cfg(unix)]
    if f.metadata().is_ok_and(|m| m.is_dir()) {
        return Err(path_error(&std::io::Error::from_raw_os_error(libc::EISDIR), p));
    }
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    Ok(universal_newlines(decode_utf8(bytes)?))
}

/// tempfile's `_RandomNameSequence`: eight characters of [a-z0-9_].
#[cfg(unix)]
fn candidate_name() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    const NAME_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNT.fetch_add(1, Ordering::Relaxed));
    let mut x = h.finish();
    (0..8)
        .map(|_| {
            let c = NAME_CHARS[(x % NAME_CHARS.len() as u64) as usize] as char;
            x /= NAME_CHARS.len() as u64;
            c
        })
        .collect()
}

/// `tempfile.gettempdir()`: the first of $TMPDIR, $TEMP, $TMP, /tmp, /var/tmp, /usr/tmp and the working folder in
/// which a file can be created, made absolute; found once per process.
#[cfg(unix)]
pub fn gettempdir() -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    if let Some(dir) = DIR.get() {
        return Ok(dir.clone());
    }
    let mut dirs: Vec<PathBuf> =
        ["TMPDIR", "TEMP", "TMP"].iter().filter_map(std::env::var_os).filter(|v| !v.is_empty()).map(PathBuf::from).collect();
    dirs.extend(["/tmp", "/var/tmp", "/usr/tmp"].map(PathBuf::from));
    dirs.push(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    for dir in &dirs {
        let dir = if dir.as_os_str() == "." { dir.clone() } else { abspath(dir)? };
        for _ in 0..100 {
            let name = dir.join(candidate_name());
            let opened = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW)
                .mode(0o600)
                .open(&name);
            match opened {
                Ok(mut f) => {
                    let wrote = f.write_all(b"blat");
                    drop(f);
                    let removed = std::fs::remove_file(&name);
                    if wrote.is_err() || removed.is_err() {
                        break;
                    }
                    return Ok(DIR.get_or_init(|| dir).clone());
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => break,
            }
        }
    }
    let list = dirs.iter().map(|d| repr(&d.to_string_lossy())).collect::<Vec<_>>().join(", ");
    Err(Error::os(format!("[Errno 2] No usable temporary directory found in [{list}]")))
}

/// `tempfile.mkdtemp(prefix=prefix)`: a new folder, mode 0700, in gettempdir.
#[cfg(unix)]
pub fn mkdtemp(prefix: &str) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = gettempdir()?;
    for _ in 0..10_000 {
        let path = dir.join(format!("{prefix}{}", candidate_name()));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(path_error(&e, &path)),
        }
    }
    Err(Error::os("[Errno 17] No usable temporary directory name found"))
}

#[cfg(not(unix))]
pub fn mkdtemp(_prefix: &str) -> Result<PathBuf> {
    Err(Error::os(crate::rt::NATIVE_ONLY))
}

/// `tempfile.mkstemp(prefix=prefix, dir=dir)`: a new file, mode 0600, never through a symlink, and its absolute path.
#[cfg(unix)]
pub fn mkstemp(prefix: &str, dir: &Path) -> Result<(std::fs::File, PathBuf)> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = abspath(dir)?;
    for _ in 0..10_000 {
        let path = dir.join(format!("{prefix}{}", candidate_name()));
        let opened =
            std::fs::OpenOptions::new().read(true).write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).mode(0o600).open(&path);
        match opened {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(path_error(&e, &path)),
        }
    }
    Err(Error::os("[Errno 17] No usable temporary file name found"))
}

/// `TemporaryDirectory` cleanup: everything under path removed, permissions and (macOS) file flags that block the
/// removal reset first, as tempfile's error handler does; a folder already gone is no error.
#[cfg(unix)]
pub fn rmtree(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }
    // ceiling: the reset walks paths, so a process that escaped the session could race it with symlinks; it only
    // changes modes (never through a symlink), and the removal itself is std's symlink-safe one.
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        reset_perms(&p);
        if meta.is_dir()
            && let Ok(entries) = std::fs::read_dir(&p)
        {
            stack.extend(entries.flatten().map(|e| e.path()));
        }
    }
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(path_error(&e, path)),
    }
}

/// tempfile's `_resetperms` for a path that is not a symlink: no file flags, mode 0700. macOS changes the link
/// itself if the path became one meanwhile (lchflags, lchmod); elsewhere Python checks, then follows, and so does this.
#[cfg(unix)]
fn reset_perms(p: &Path) {
    let Ok(c) = std::ffi::CString::new(bytes_of(p)) else { return };
    // SAFETY: c is a valid NUL-terminated path for each call.
    unsafe {
        #[cfg(target_os = "macos")]
        {
            libc::lchflags(c.as_ptr(), 0);
            libc::fchmodat(libc::AT_FDCWD, c.as_ptr(), 0o700, libc::AT_SYMLINK_NOFOLLOW);
        }
        #[cfg(not(target_os = "macos"))]
        libc::chmod(c.as_ptr(), 0o700);
    }
}

/// The text Python prints for an OSError: `[Errno N] <strerror>` and, given a file name, `: '<name>'`.
pub fn os_error(e: &std::io::Error, filename: Option<&str>) -> String {
    let base = match e.raw_os_error() {
        Some(code) => format!("[Errno {code}] {}", strerror(code)),
        None => e.to_string(),
    };
    match filename {
        Some(name) => format!("{base}: {}", repr(name)),
        None => base,
    }
}

fn strerror(code: i32) -> String {
    #[cfg(unix)]
    unsafe {
        let p = libc::strerror(code);
        if !p.is_null() {
            return std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
        }
    }
    let text = std::io::Error::from_raw_os_error(code).to_string();
    text.strip_suffix(&format!(" (os error {code})")).unwrap_or(&text).to_string()
}

/// textwrap.TextWrapper(width, initial_indent, subsequent_indent).wrap(text) for text whose whitespace argparse
/// has already collapsed to single spaces (break_long_words and break_on_hyphens on, as by default).
pub mod textwrap {
    use super::{is_word, decimal_digit, strip};

    fn ws(c: char) -> bool {
        matches!(c, '\t' | '\n' | '\x0b' | '\x0c' | '\r' | ' ')
    }

    fn word_punct(c: char) -> bool {
        is_word(c) || "!\"'&.,?".contains(c)
    }

    /// `[^\d\W]`.
    fn letter(c: char) -> bool {
        is_word(c) && decimal_digit(c).is_none()
    }

    /// The run of dashes at i, of two or more, followed by a word character.
    fn em_dash(c: &[char], i: usize) -> bool {
        let mut j = i;
        while j < c.len() && c[j] == '-' {
            j += 1;
        }
        j - i >= 2 && j < c.len() && is_word(c[j])
    }

    /// `TextWrapper.wordsep_re.split(text)` without the empty strings.
    pub fn chunks(text: &str) -> Vec<String> {
        let c: Vec<char> = text.chars().map(|ch| if ws(ch) { ' ' } else { ch }).collect();
        let at = |i: usize| c.get(i).copied();
        let lt = |i: usize| at(i).is_some_and(letter);
        let mut out = Vec::new();
        let mut p = 0;
        while p < c.len() {
            let end = if ws(c[p]) {
                let mut q = p;
                while q < c.len() && ws(c[q]) {
                    q += 1;
                }
                q
            } else if c[p] == '-' && p > 0 && word_punct(c[p - 1]) && em_dash(&c, p) {
                let mut q = p;
                while c[q] == '-' {
                    q += 1;
                }
                q
            } else {
                let mut e = p + 1;
                loop {
                    let hyphen = at(e) == Some('-')
                        && ((e >= 2 && lt(e - 2) && lt(e - 1)) || (e >= 3 && lt(e - 3) && at(e - 2) == Some('-') && lt(e - 1)))
                        && lt(e + 1)
                        && (lt(e + 2) || (at(e + 2) == Some('-') && lt(e + 3)));
                    if hyphen {
                        break e + 1;
                    }
                    if e == c.len() || ws(c[e]) || (word_punct(c[e - 1]) && em_dash(&c, e)) {
                        break e;
                    }
                    e += 1;
                }
            };
            out.push(c[p..end].iter().collect());
            p = end;
        }
        out
    }

    fn blank(s: &str) -> bool {
        strip(s).is_empty()
    }

    fn len(s: &str) -> usize {
        s.chars().count()
    }

    pub fn wrap(text: &str, width: i64, initial_indent: &str, subsequent_indent: &str) -> Vec<String> {
        let mut chunks = chunks(text);
        chunks.reverse();
        let mut lines: Vec<String> = Vec::new();
        while !chunks.is_empty() {
            let mut cur: Vec<String> = Vec::new();
            let mut cur_len: i64 = 0;
            let indent = if lines.is_empty() { initial_indent } else { subsequent_indent };
            let width = width - len(indent) as i64;
            if blank(chunks.last().expect("a chunk")) && !lines.is_empty() {
                chunks.pop();
            }
            while let Some(last) = chunks.last() {
                let l = len(last) as i64;
                if cur_len + l <= width {
                    cur.push(chunks.pop().expect("a chunk"));
                    cur_len += l;
                } else {
                    break;
                }
            }
            let long = chunks.last().is_some_and(|c| len(c) as i64 > width);
            if long {
                // _handle_long_word
                let space_left = if width < 1 { 1 } else { width - cur_len };
                if space_left > 0 {
                    let chunk: Vec<char> = chunks.last().expect("a chunk").chars().collect();
                    let mut end = space_left as usize;
                    if chunk.len() > end {
                        let hyphen = chunk[..end].iter().rposition(|&c| c == '-');
                        if let Some(h) = hyphen
                            && h > 0
                            && chunk[..h].iter().any(|&c| c != '-')
                        {
                            end = h + 1;
                        }
                    }
                    let end = end.min(chunk.len());
                    cur.push(chunk[..end].iter().collect());
                    *chunks.last_mut().expect("a chunk") = chunk[end..].iter().collect();
                } else if cur.is_empty() {
                    cur.push(chunks.pop().expect("a chunk"));
                }
                cur_len = cur.iter().map(|c| len(c) as i64).sum();
            }
            if cur.last().is_some_and(|c| blank(c)) {
                cur_len -= len(cur.last().expect("a chunk")) as i64;
                cur.pop();
            }
            let _ = cur_len;
            if !cur.is_empty() {
                lines.push(format!("{indent}{}", cur.concat()));
            }
        }
        lines
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Unicode tables (ranges of hex code points), generated with Python's str.isalnum() / '_' (WORD_TABLE),
// str.isdecimal() (DECIMAL_TABLE), not str.isprintable() (UNPRINTABLE_TABLE) and str.upper() != str (UPPER_TABLE).
// Generated from Python 3.14.7, Unicode 16.0.0.
const WORD_TABLE: &str = "\
30-39 41-5A 5F 61-7A AA B2-B3 B5 B9-BA BC-BE C0-D6 D8-F6 F8-2C1 2C6-2D1 2E0-2E4 2EC 2EE 370-374 376-377 \
37A-37D 37F 386 388-38A 38C 38E-3A1 3A3-3F5 3F7-481 48A-52F 531-556 559 560-588 5D0-5EA 5EF-5F2 620-64A \
660-669 66E-66F 671-6D3 6D5 6E5-6E6 6EE-6FC 6FF 710 712-72F 74D-7A5 7B1 7C0-7EA 7F4-7F5 7FA 800-815 81A 824 \
828 840-858 860-86A 870-887 889-88E 8A0-8C9 904-939 93D 950 958-961 966-96F 971-980 985-98C 98F-990 993-9A8 \
9AA-9B0 9B2 9B6-9B9 9BD 9CE 9DC-9DD 9DF-9E1 9E6-9F1 9F4-9F9 9FC A05-A0A A0F-A10 A13-A28 A2A-A30 A32-A33 \
A35-A36 A38-A39 A59-A5C A5E A66-A6F A72-A74 A85-A8D A8F-A91 A93-AA8 AAA-AB0 AB2-AB3 AB5-AB9 ABD AD0 AE0-AE1 \
AE6-AEF AF9 B05-B0C B0F-B10 B13-B28 B2A-B30 B32-B33 B35-B39 B3D B5C-B5D B5F-B61 B66-B6F B71-B77 B83 B85-B8A \
B8E-B90 B92-B95 B99-B9A B9C B9E-B9F BA3-BA4 BA8-BAA BAE-BB9 BD0 BE6-BF2 C05-C0C C0E-C10 C12-C28 C2A-C39 C3D \
C58-C5A C5D C60-C61 C66-C6F C78-C7E C80 C85-C8C C8E-C90 C92-CA8 CAA-CB3 CB5-CB9 CBD CDD-CDE CE0-CE1 CE6-CEF \
CF1-CF2 D04-D0C D0E-D10 D12-D3A D3D D4E D54-D56 D58-D61 D66-D78 D7A-D7F D85-D96 D9A-DB1 DB3-DBB DBD DC0-DC6 \
DE6-DEF E01-E30 E32-E33 E40-E46 E50-E59 E81-E82 E84 E86-E8A E8C-EA3 EA5 EA7-EB0 EB2-EB3 EBD EC0-EC4 EC6 \
ED0-ED9 EDC-EDF F00 F20-F33 F40-F47 F49-F6C F88-F8C 1000-102A 103F-1049 1050-1055 105A-105D 1061 1065-1066 \
106E-1070 1075-1081 108E 1090-1099 10A0-10C5 10C7 10CD 10D0-10FA 10FC-1248 124A-124D 1250-1256 1258 125A-125D \
1260-1288 128A-128D 1290-12B0 12B2-12B5 12B8-12BE 12C0 12C2-12C5 12C8-12D6 12D8-1310 1312-1315 1318-135A \
1369-137C 1380-138F 13A0-13F5 13F8-13FD 1401-166C 166F-167F 1681-169A 16A0-16EA 16EE-16F8 1700-1711 171F-1731 \
1740-1751 1760-176C 176E-1770 1780-17B3 17D7 17DC 17E0-17E9 17F0-17F9 1810-1819 1820-1878 1880-1884 1887-18A8 \
18AA 18B0-18F5 1900-191E 1946-196D 1970-1974 1980-19AB 19B0-19C9 19D0-19DA 1A00-1A16 1A20-1A54 1A80-1A89 \
1A90-1A99 1AA7 1B05-1B33 1B45-1B4C 1B50-1B59 1B83-1BA0 1BAE-1BE5 1C00-1C23 1C40-1C49 1C4D-1C7D 1C80-1C8A \
1C90-1CBA 1CBD-1CBF 1CE9-1CEC 1CEE-1CF3 1CF5-1CF6 1CFA 1D00-1DBF 1E00-1F15 1F18-1F1D 1F20-1F45 1F48-1F4D \
1F50-1F57 1F59 1F5B 1F5D 1F5F-1F7D 1F80-1FB4 1FB6-1FBC 1FBE 1FC2-1FC4 1FC6-1FCC 1FD0-1FD3 1FD6-1FDB 1FE0-1FEC \
1FF2-1FF4 1FF6-1FFC 2070-2071 2074-2079 207F-2089 2090-209C 2102 2107 210A-2113 2115 2119-211D 2124 2126 2128 \
212A-212D 212F-2139 213C-213F 2145-2149 214E 2150-2189 2460-249B 24EA-24FF 2776-2793 2C00-2CE4 2CEB-2CEE \
2CF2-2CF3 2CFD 2D00-2D25 2D27 2D2D 2D30-2D67 2D6F 2D80-2D96 2DA0-2DA6 2DA8-2DAE 2DB0-2DB6 2DB8-2DBE 2DC0-2DC6 \
2DC8-2DCE 2DD0-2DD6 2DD8-2DDE 2E2F 3005-3007 3021-3029 3031-3035 3038-303C 3041-3096 309D-309F 30A1-30FA \
30FC-30FF 3105-312F 3131-318E 3192-3195 31A0-31BF 31F0-31FF 3220-3229 3248-324F 3251-325F 3280-3289 32B1-32BF \
3400-4DBF 4E00-A48C A4D0-A4FD A500-A60C A610-A62B A640-A66E A67F-A69D A6A0-A6EF A717-A71F A722-A788 A78B-A7CD \
A7D0-A7D1 A7D3 A7D5-A7DC A7F2-A801 A803-A805 A807-A80A A80C-A822 A830-A835 A840-A873 A882-A8B3 A8D0-A8D9 \
A8F2-A8F7 A8FB A8FD-A8FE A900-A925 A930-A946 A960-A97C A984-A9B2 A9CF-A9D9 A9E0-A9E4 A9E6-A9FE AA00-AA28 \
AA40-AA42 AA44-AA4B AA50-AA59 AA60-AA76 AA7A AA7E-AAAF AAB1 AAB5-AAB6 AAB9-AABD AAC0 AAC2 AADB-AADD AAE0-AAEA \
AAF2-AAF4 AB01-AB06 AB09-AB0E AB11-AB16 AB20-AB26 AB28-AB2E AB30-AB5A AB5C-AB69 AB70-ABE2 ABF0-ABF9 AC00-D7A3 \
D7B0-D7C6 D7CB-D7FB F900-FA6D FA70-FAD9 FB00-FB06 FB13-FB17 FB1D FB1F-FB28 FB2A-FB36 FB38-FB3C FB3E FB40-FB41 \
FB43-FB44 FB46-FBB1 FBD3-FD3D FD50-FD8F FD92-FDC7 FDF0-FDFB FE70-FE74 FE76-FEFC FF10-FF19 FF21-FF3A FF41-FF5A \
FF66-FFBE FFC2-FFC7 FFCA-FFCF FFD2-FFD7 FFDA-FFDC 10000-1000B 1000D-10026 10028-1003A 1003C-1003D 1003F-1004D \
10050-1005D 10080-100FA 10107-10133 10140-10178 1018A-1018B 10280-1029C 102A0-102D0 102E1-102FB 10300-10323 \
1032D-1034A 10350-10375 10380-1039D 103A0-103C3 103C8-103CF 103D1-103D5 10400-1049D 104A0-104A9 104B0-104D3 \
104D8-104FB 10500-10527 10530-10563 10570-1057A 1057C-1058A 1058C-10592 10594-10595 10597-105A1 105A3-105B1 \
105B3-105B9 105BB-105BC 105C0-105F3 10600-10736 10740-10755 10760-10767 10780-10785 10787-107B0 107B2-107BA \
10800-10805 10808 1080A-10835 10837-10838 1083C 1083F-10855 10858-10876 10879-1089E 108A7-108AF 108E0-108F2 \
108F4-108F5 108FB-1091B 10920-10939 10980-109B7 109BC-109CF 109D2-10A00 10A10-10A13 10A15-10A17 10A19-10A35 \
10A40-10A48 10A60-10A7E 10A80-10A9F 10AC0-10AC7 10AC9-10AE4 10AEB-10AEF 10B00-10B35 10B40-10B55 10B58-10B72 \
10B78-10B91 10BA9-10BAF 10C00-10C48 10C80-10CB2 10CC0-10CF2 10CFA-10D23 10D30-10D39 10D40-10D65 10D6F-10D85 \
10E60-10E7E 10E80-10EA9 10EB0-10EB1 10EC2-10EC4 10F00-10F27 10F30-10F45 10F51-10F54 10F70-10F81 10FB0-10FCB \
10FE0-10FF6 11003-11037 11052-1106F 11071-11072 11075 11083-110AF 110D0-110E8 110F0-110F9 11103-11126 \
11136-1113F 11144 11147 11150-11172 11176 11183-111B2 111C1-111C4 111D0-111DA 111DC 111E1-111F4 11200-11211 \
11213-1122B 1123F-11240 11280-11286 11288 1128A-1128D 1128F-1129D 1129F-112A8 112B0-112DE 112F0-112F9 \
11305-1130C 1130F-11310 11313-11328 1132A-11330 11332-11333 11335-11339 1133D 11350 1135D-11361 11380-11389 \
1138B 1138E 11390-113B5 113B7 113D1 113D3 11400-11434 11447-1144A 11450-11459 1145F-11461 11480-114AF \
114C4-114C5 114C7 114D0-114D9 11580-115AE 115D8-115DB 11600-1162F 11644 11650-11659 11680-116AA 116B8 \
116C0-116C9 116D0-116E3 11700-1171A 11730-1173B 11740-11746 11800-1182B 118A0-118F2 118FF-11906 11909 \
1190C-11913 11915-11916 11918-1192F 1193F 11941 11950-11959 119A0-119A7 119AA-119D0 119E1 119E3 11A00 \
11A0B-11A32 11A3A 11A50 11A5C-11A89 11A9D 11AB0-11AF8 11BC0-11BE0 11BF0-11BF9 11C00-11C08 11C0A-11C2E 11C40 \
11C50-11C6C 11C72-11C8F 11D00-11D06 11D08-11D09 11D0B-11D30 11D46 11D50-11D59 11D60-11D65 11D67-11D68 \
11D6A-11D89 11D98 11DA0-11DA9 11EE0-11EF2 11F02 11F04-11F10 11F12-11F33 11F50-11F59 11FB0 11FC0-11FD4 \
12000-12399 12400-1246E 12480-12543 12F90-12FF0 13000-1342F 13441-13446 13460-143FA 14400-14646 16100-1611D \
16130-16139 16800-16A38 16A40-16A5E 16A60-16A69 16A70-16ABE 16AC0-16AC9 16AD0-16AED 16B00-16B2F 16B40-16B43 \
16B50-16B59 16B5B-16B61 16B63-16B77 16B7D-16B8F 16D40-16D6C 16D70-16D79 16E40-16E96 16F00-16F4A 16F50 \
16F93-16F9F 16FE0-16FE1 16FE3 17000-187F7 18800-18CD5 18CFF-18D08 1AFF0-1AFF3 1AFF5-1AFFB 1AFFD-1AFFE \
1B000-1B122 1B132 1B150-1B152 1B155 1B164-1B167 1B170-1B2FB 1BC00-1BC6A 1BC70-1BC7C 1BC80-1BC88 1BC90-1BC99 \
1CCF0-1CCF9 1D2C0-1D2D3 1D2E0-1D2F3 1D360-1D378 1D400-1D454 1D456-1D49C 1D49E-1D49F 1D4A2 1D4A5-1D4A6 \
1D4A9-1D4AC 1D4AE-1D4B9 1D4BB 1D4BD-1D4C3 1D4C5-1D505 1D507-1D50A 1D50D-1D514 1D516-1D51C 1D51E-1D539 \
1D53B-1D53E 1D540-1D544 1D546 1D54A-1D550 1D552-1D6A5 1D6A8-1D6C0 1D6C2-1D6DA 1D6DC-1D6FA 1D6FC-1D714 \
1D716-1D734 1D736-1D74E 1D750-1D76E 1D770-1D788 1D78A-1D7A8 1D7AA-1D7C2 1D7C4-1D7CB 1D7CE-1D7FF 1DF00-1DF1E \
1DF25-1DF2A 1E030-1E06D 1E100-1E12C 1E137-1E13D 1E140-1E149 1E14E 1E290-1E2AD 1E2C0-1E2EB 1E2F0-1E2F9 \
1E4D0-1E4EB 1E4F0-1E4F9 1E5D0-1E5ED 1E5F0-1E5FA 1E7E0-1E7E6 1E7E8-1E7EB 1E7ED-1E7EE 1E7F0-1E7FE 1E800-1E8C4 \
1E8C7-1E8CF 1E900-1E943 1E94B 1E950-1E959 1EC71-1ECAB 1ECAD-1ECAF 1ECB1-1ECB4 1ED01-1ED2D 1ED2F-1ED3D \
1EE00-1EE03 1EE05-1EE1F 1EE21-1EE22 1EE24 1EE27 1EE29-1EE32 1EE34-1EE37 1EE39 1EE3B 1EE42 1EE47 1EE49 1EE4B \
1EE4D-1EE4F 1EE51-1EE52 1EE54 1EE57 1EE59 1EE5B 1EE5D 1EE5F 1EE61-1EE62 1EE64 1EE67-1EE6A 1EE6C-1EE72 \
1EE74-1EE77 1EE79-1EE7C 1EE7E 1EE80-1EE89 1EE8B-1EE9B 1EEA1-1EEA3 1EEA5-1EEA9 1EEAB-1EEBB 1F100-1F10C \
1FBF0-1FBF9 20000-2A6DF 2A700-2B739 2B740-2B81D 2B820-2CEA1 2CEB0-2EBE0 2EBF0-2EE5D 2F800-2FA1D 30000-3134A \
31350-323AF";
const DECIMAL_TABLE: &str = "\
30-39 660-669 6F0-6F9 7C0-7C9 966-96F 9E6-9EF A66-A6F AE6-AEF B66-B6F BE6-BEF C66-C6F CE6-CEF D66-D6F DE6-DEF \
E50-E59 ED0-ED9 F20-F29 1040-1049 1090-1099 17E0-17E9 1810-1819 1946-194F 19D0-19D9 1A80-1A89 1A90-1A99 \
1B50-1B59 1BB0-1BB9 1C40-1C49 1C50-1C59 A620-A629 A8D0-A8D9 A900-A909 A9D0-A9D9 A9F0-A9F9 AA50-AA59 ABF0-ABF9 \
FF10-FF19 104A0-104A9 10D30-10D39 10D40-10D49 11066-1106F 110F0-110F9 11136-1113F 111D0-111D9 112F0-112F9 \
11450-11459 114D0-114D9 11650-11659 116C0-116C9 116D0-116E3 11730-11739 118E0-118E9 11950-11959 11BF0-11BF9 \
11C50-11C59 11D50-11D59 11DA0-11DA9 11F50-11F59 16130-16139 16A60-16A69 16AC0-16AC9 16B50-16B59 16D70-16D79 \
1CCF0-1CCF9 1D7CE-1D7FF 1E140-1E149 1E2F0-1E2F9 1E4F0-1E4F9 1E5F1-1E5FA 1E950-1E959 1FBF0-1FBF9";
const UNPRINTABLE_TABLE: &str = "\
0-1F 7F-A0 AD 378-379 380-383 38B 38D 3A2 530 557-558 58B-58C 590 5C8-5CF 5EB-5EE 5F5-605 61C 6DD 70E-70F \
74B-74C 7B2-7BF 7FB-7FC 82E-82F 83F 85C-85D 85F 86B-86F 88F-896 8E2 984 98D-98E 991-992 9A9 9B1 9B3-9B5 \
9BA-9BB 9C5-9C6 9C9-9CA 9CF-9D6 9D8-9DB 9DE 9E4-9E5 9FF-A00 A04 A0B-A0E A11-A12 A29 A31 A34 A37 A3A-A3B A3D \
A43-A46 A49-A4A A4E-A50 A52-A58 A5D A5F-A65 A77-A80 A84 A8E A92 AA9 AB1 AB4 ABA-ABB AC6 ACA ACE-ACF AD1-ADF \
AE4-AE5 AF2-AF8 B00 B04 B0D-B0E B11-B12 B29 B31 B34 B3A-B3B B45-B46 B49-B4A B4E-B54 B58-B5B B5E B64-B65 \
B78-B81 B84 B8B-B8D B91 B96-B98 B9B B9D BA0-BA2 BA5-BA7 BAB-BAD BBA-BBD BC3-BC5 BC9 BCE-BCF BD1-BD6 BD8-BE5 \
BFB-BFF C0D C11 C29 C3A-C3B C45 C49 C4E-C54 C57 C5B-C5C C5E-C5F C64-C65 C70-C76 C8D C91 CA9 CB4 CBA-CBB CC5 \
CC9 CCE-CD4 CD7-CDC CDF CE4-CE5 CF0 CF4-CFF D0D D11 D45 D49 D50-D53 D64-D65 D80 D84 D97-D99 DB2 DBC DBE-DBF \
DC7-DC9 DCB-DCE DD5 DD7 DE0-DE5 DF0-DF1 DF5-E00 E3B-E3E E5C-E80 E83 E85 E8B EA4 EA6 EBE-EBF EC5 EC7 ECF \
EDA-EDB EE0-EFF F48 F6D-F70 F98 FBD FCD FDB-FFF 10C6 10C8-10CC 10CE-10CF 1249 124E-124F 1257 1259 125E-125F \
1289 128E-128F 12B1 12B6-12B7 12BF 12C1 12C6-12C7 12D7 1311 1316-1317 135B-135C 137D-137F 139A-139F 13F6-13F7 \
13FE-13FF 1680 169D-169F 16F9-16FF 1716-171E 1737-173F 1754-175F 176D 1771 1774-177F 17DE-17DF 17EA-17EF \
17FA-17FF 180E 181A-181F 1879-187F 18AB-18AF 18F6-18FF 191F 192C-192F 193C-193F 1941-1943 196E-196F 1975-197F \
19AC-19AF 19CA-19CF 19DB-19DD 1A1C-1A1D 1A5F 1A7D-1A7E 1A8A-1A8F 1A9A-1A9F 1AAE-1AAF 1ACF-1AFF 1B4D 1BF4-1BFB \
1C38-1C3A 1C4A-1C4C 1C8B-1C8F 1CBB-1CBC 1CC8-1CCF 1CFB-1CFF 1F16-1F17 1F1E-1F1F 1F46-1F47 1F4E-1F4F 1F58 1F5A \
1F5C 1F5E 1F7E-1F7F 1FB5 1FC5 1FD4-1FD5 1FDC 1FF0-1FF1 1FF5 1FFF-200F 2028-202F 205F-206F 2072-2073 208F \
209D-209F 20C1-20CF 20F1-20FF 218C-218F 242A-243F 244B-245F 2B74-2B75 2B96 2CF4-2CF8 2D26 2D28-2D2C 2D2E-2D2F \
2D68-2D6E 2D71-2D7E 2D97-2D9F 2DA7 2DAF 2DB7 2DBF 2DC7 2DCF 2DD7 2DDF 2E5E-2E7F 2E9A 2EF4-2EFF 2FD6-2FEF 3000 \
3040 3097-3098 3100-3104 3130 318F 31E6-31EE 321F A48D-A48F A4C7-A4CF A62C-A63F A6F8-A6FF A7CE-A7CF A7D2 A7D4 \
A7DD-A7F1 A82D-A82F A83A-A83F A878-A87F A8C6-A8CD A8DA-A8DF A954-A95E A97D-A97F A9CE A9DA-A9DD A9FF AA37-AA3F \
AA4E-AA4F AA5A-AA5B AAC3-AADA AAF7-AB00 AB07-AB08 AB0F-AB10 AB17-AB1F AB27 AB2F AB6C-AB6F ABEE-ABEF ABFA-ABFF \
D7A4-D7AF D7C7-D7CA D7FC-F8FF FA6E-FA6F FADA-FAFF FB07-FB12 FB18-FB1C FB37 FB3D FB3F FB42 FB45 FBC3-FBD2 \
FD90-FD91 FDC8-FDCE FDD0-FDEF FE1A-FE1F FE53 FE67 FE6C-FE6F FE75 FEFD-FF00 FFBF-FFC1 FFC8-FFC9 FFD0-FFD1 \
FFD8-FFD9 FFDD-FFDF FFE7 FFEF-FFFB FFFE-FFFF 1000C 10027 1003B 1003E 1004E-1004F 1005E-1007F 100FB-100FF \
10103-10106 10134-10136 1018F 1019D-1019F 101A1-101CF 101FE-1027F 1029D-1029F 102D1-102DF 102FC-102FF \
10324-1032C 1034B-1034F 1037B-1037F 1039E 103C4-103C7 103D6-103FF 1049E-1049F 104AA-104AF 104D4-104D7 \
104FC-104FF 10528-1052F 10564-1056E 1057B 1058B 10593 10596 105A2 105B2 105BA 105BD-105BF 105F4-105FF \
10737-1073F 10756-1075F 10768-1077F 10786 107B1 107BB-107FF 10806-10807 10809 10836 10839-1083B 1083D-1083E \
10856 1089F-108A6 108B0-108DF 108F3 108F6-108FA 1091C-1091E 1093A-1093E 10940-1097F 109B8-109BB 109D0-109D1 \
10A04 10A07-10A0B 10A14 10A18 10A36-10A37 10A3B-10A3E 10A49-10A4F 10A59-10A5F 10AA0-10ABF 10AE7-10AEA \
10AF7-10AFF 10B36-10B38 10B56-10B57 10B73-10B77 10B92-10B98 10B9D-10BA8 10BB0-10BFF 10C49-10C7F 10CB3-10CBF \
10CF3-10CF9 10D28-10D2F 10D3A-10D3F 10D66-10D68 10D86-10D8D 10D90-10E5F 10E7F 10EAA 10EAE-10EAF 10EB2-10EC1 \
10EC5-10EFB 10F28-10F2F 10F5A-10F6F 10F8A-10FAF 10FCC-10FDF 10FF7-10FFF 1104E-11051 11076-1107E 110BD \
110C3-110CF 110E9-110EF 110FA-110FF 11135 11148-1114F 11177-1117F 111E0 111F5-111FF 11212 11242-1127F 11287 \
11289 1128E 1129E 112AA-112AF 112EB-112EF 112FA-112FF 11304 1130D-1130E 11311-11312 11329 11331 11334 1133A \
11345-11346 11349-1134A 1134E-1134F 11351-11356 11358-1135C 11364-11365 1136D-1136F 11375-1137F 1138A \
1138C-1138D 1138F 113B6 113C1 113C3-113C4 113C6 113CB 113D6 113D9-113E0 113E3-113FF 1145C 11462-1147F \
114C8-114CF 114DA-1157F 115B6-115B7 115DE-115FF 11645-1164F 1165A-1165F 1166D-1167F 116BA-116BF 116CA-116CF \
116E4-116FF 1171B-1171C 1172C-1172F 11747-117FF 1183C-1189F 118F3-118FE 11907-11908 1190A-1190B 11914 11917 \
11936 11939-1193A 11947-1194F 1195A-1199F 119A8-119A9 119D8-119D9 119E5-119FF 11A48-11A4F 11AA3-11AAF \
11AF9-11AFF 11B0A-11BBF 11BE2-11BEF 11BFA-11BFF 11C09 11C37 11C46-11C4F 11C6D-11C6F 11C90-11C91 11CA8 \
11CB7-11CFF 11D07 11D0A 11D37-11D39 11D3B 11D3E 11D48-11D4F 11D5A-11D5F 11D66 11D69 11D8F 11D92 11D99-11D9F \
11DAA-11EDF 11EF9-11EFF 11F11 11F3B-11F3D 11F5B-11FAF 11FB1-11FBF 11FF2-11FFE 1239A-123FF 1246F 12475-1247F \
12544-12F8F 12FF3-12FFF 13430-1343F 13456-1345F 143FB-143FF 14647-160FF 1613A-167FF 16A39-16A3F 16A5F \
16A6A-16A6D 16ABF 16ACA-16ACF 16AEE-16AEF 16AF6-16AFF 16B46-16B4F 16B5A 16B62 16B78-16B7C 16B90-16D3F \
16D7A-16E3F 16E9B-16EFF 16F4B-16F4E 16F88-16F8E 16FA0-16FDF 16FE5-16FEF 16FF2-16FFF 187F8-187FF 18CD6-18CFE \
18D09-1AFEF 1AFF4 1AFFC 1AFFF 1B123-1B131 1B133-1B14F 1B153-1B154 1B156-1B163 1B168-1B16F 1B2FC-1BBFF \
1BC6B-1BC6F 1BC7D-1BC7F 1BC89-1BC8F 1BC9A-1BC9B 1BCA0-1CBFF 1CCFA-1CCFF 1CEB4-1CEFF 1CF2E-1CF2F 1CF47-1CF4F \
1CFC4-1CFFF 1D0F6-1D0FF 1D127-1D128 1D173-1D17A 1D1EB-1D1FF 1D246-1D2BF 1D2D4-1D2DF 1D2F4-1D2FF 1D357-1D35F \
1D379-1D3FF 1D455 1D49D 1D4A0-1D4A1 1D4A3-1D4A4 1D4A7-1D4A8 1D4AD 1D4BA 1D4BC 1D4C4 1D506 1D50B-1D50C 1D515 \
1D51D 1D53A 1D53F 1D545 1D547-1D549 1D551 1D6A6-1D6A7 1D7CC-1D7CD 1DA8C-1DA9A 1DAA0 1DAB0-1DEFF 1DF1F-1DF24 \
1DF2B-1DFFF 1E007 1E019-1E01A 1E022 1E025 1E02B-1E02F 1E06E-1E08E 1E090-1E0FF 1E12D-1E12F 1E13E-1E13F \
1E14A-1E14D 1E150-1E28F 1E2AF-1E2BF 1E2FA-1E2FE 1E300-1E4CF 1E4FA-1E5CF 1E5FB-1E5FE 1E600-1E7DF 1E7E7 1E7EC \
1E7EF 1E7FF 1E8C5-1E8C6 1E8D7-1E8FF 1E94C-1E94F 1E95A-1E95D 1E960-1EC70 1ECB5-1ED00 1ED3E-1EDFF 1EE04 1EE20 \
1EE23 1EE25-1EE26 1EE28 1EE33 1EE38 1EE3A 1EE3C-1EE41 1EE43-1EE46 1EE48 1EE4A 1EE4C 1EE50 1EE53 1EE55-1EE56 \
1EE58 1EE5A 1EE5C 1EE5E 1EE60 1EE63 1EE65-1EE66 1EE6B 1EE73 1EE78 1EE7D 1EE7F 1EE8A 1EE9C-1EEA0 1EEA4 1EEAA \
1EEBC-1EEEF 1EEF2-1EFFF 1F02C-1F02F 1F094-1F09F 1F0AF-1F0B0 1F0C0 1F0D0 1F0F6-1F0FF 1F1AE-1F1E5 1F203-1F20F \
1F23C-1F23F 1F249-1F24F 1F252-1F25F 1F266-1F2FF 1F6D8-1F6DB 1F6ED-1F6EF 1F6FD-1F6FF 1F777-1F77A 1F7DA-1F7DF \
1F7EC-1F7EF 1F7F1-1F7FF 1F80C-1F80F 1F848-1F84F 1F85A-1F85F 1F888-1F88F 1F8AE-1F8AF 1F8BC-1F8BF 1F8C2-1F8FF \
1FA54-1FA5F 1FA6E-1FA6F 1FA7D-1FA7F 1FA8A-1FA8E 1FAC7-1FACD 1FADD-1FADE 1FAEA-1FAEF 1FAF9-1FAFF 1FB93 \
1FBFA-1FFFF 2A6E0-2A6FF 2B73A-2B73F 2B81E-2B81F 2CEA2-2CEAF 2EBE1-2EBEF 2EE5E-2F7FF 2FA1E-2FFFF 3134B-3134F \
323B0-E00FF E01F0-10FFFF";
const UPPER_TABLE: &str = "\
61-7A B5 DF-F6 F8-FF 101 103 105 107 109 10B 10D 10F 111 113 115 117 119 11B 11D 11F 121 123 125 127 129 12B \
12D 12F 131 133 135 137 13A 13C 13E 140 142 144 146 148-149 14B 14D 14F 151 153 155 157 159 15B 15D 15F 161 \
163 165 167 169 16B 16D 16F 171 173 175 177 17A 17C 17E-180 183 185 188 18C 192 195 199-19B 19E 1A1 1A3 1A5 \
1A8 1AD 1B0 1B4 1B6 1B9 1BD 1BF 1C5-1C6 1C8-1C9 1CB-1CC 1CE 1D0 1D2 1D4 1D6 1D8 1DA 1DC-1DD 1DF 1E1 1E3 1E5 \
1E7 1E9 1EB 1ED 1EF-1F0 1F2-1F3 1F5 1F9 1FB 1FD 1FF 201 203 205 207 209 20B 20D 20F 211 213 215 217 219 21B \
21D 21F 223 225 227 229 22B 22D 22F 231 233 23C 23F-240 242 247 249 24B 24D 24F-254 256-257 259 25B-25C \
260-261 263-266 268-26C 26F 271-272 275 27D 280 282-283 287-28C 292 29D-29E 345 371 373 377 37B-37D 390 \
3AC-3CE 3D0-3D1 3D5-3D7 3D9 3DB 3DD 3DF 3E1 3E3 3E5 3E7 3E9 3EB 3ED 3EF-3F3 3F5 3F8 3FB 430-45F 461 463 465 \
467 469 46B 46D 46F 471 473 475 477 479 47B 47D 47F 481 48B 48D 48F 491 493 495 497 499 49B 49D 49F 4A1 4A3 \
4A5 4A7 4A9 4AB 4AD 4AF 4B1 4B3 4B5 4B7 4B9 4BB 4BD 4BF 4C2 4C4 4C6 4C8 4CA 4CC 4CE-4CF 4D1 4D3 4D5 4D7 4D9 \
4DB 4DD 4DF 4E1 4E3 4E5 4E7 4E9 4EB 4ED 4EF 4F1 4F3 4F5 4F7 4F9 4FB 4FD 4FF 501 503 505 507 509 50B 50D 50F \
511 513 515 517 519 51B 51D 51F 521 523 525 527 529 52B 52D 52F 561-587 10D0-10FA 10FD-10FF 13F8-13FD \
1C80-1C88 1C8A 1D79 1D7D 1D8E 1E01 1E03 1E05 1E07 1E09 1E0B 1E0D 1E0F 1E11 1E13 1E15 1E17 1E19 1E1B 1E1D 1E1F \
1E21 1E23 1E25 1E27 1E29 1E2B 1E2D 1E2F 1E31 1E33 1E35 1E37 1E39 1E3B 1E3D 1E3F 1E41 1E43 1E45 1E47 1E49 1E4B \
1E4D 1E4F 1E51 1E53 1E55 1E57 1E59 1E5B 1E5D 1E5F 1E61 1E63 1E65 1E67 1E69 1E6B 1E6D 1E6F 1E71 1E73 1E75 1E77 \
1E79 1E7B 1E7D 1E7F 1E81 1E83 1E85 1E87 1E89 1E8B 1E8D 1E8F 1E91 1E93 1E95-1E9B 1EA1 1EA3 1EA5 1EA7 1EA9 1EAB \
1EAD 1EAF 1EB1 1EB3 1EB5 1EB7 1EB9 1EBB 1EBD 1EBF 1EC1 1EC3 1EC5 1EC7 1EC9 1ECB 1ECD 1ECF 1ED1 1ED3 1ED5 1ED7 \
1ED9 1EDB 1EDD 1EDF 1EE1 1EE3 1EE5 1EE7 1EE9 1EEB 1EED 1EEF 1EF1 1EF3 1EF5 1EF7 1EF9 1EFB 1EFD 1EFF-1F07 \
1F10-1F15 1F20-1F27 1F30-1F37 1F40-1F45 1F50-1F57 1F60-1F67 1F70-1F7D 1F80-1FB4 1FB6-1FB7 1FBC 1FBE 1FC2-1FC4 \
1FC6-1FC7 1FCC 1FD0-1FD3 1FD6-1FD7 1FE0-1FE7 1FF2-1FF4 1FF6-1FF7 1FFC 214E 2170-217F 2184 24D0-24E9 2C30-2C5F \
2C61 2C65-2C66 2C68 2C6A 2C6C 2C73 2C76 2C81 2C83 2C85 2C87 2C89 2C8B 2C8D 2C8F 2C91 2C93 2C95 2C97 2C99 2C9B \
2C9D 2C9F 2CA1 2CA3 2CA5 2CA7 2CA9 2CAB 2CAD 2CAF 2CB1 2CB3 2CB5 2CB7 2CB9 2CBB 2CBD 2CBF 2CC1 2CC3 2CC5 2CC7 \
2CC9 2CCB 2CCD 2CCF 2CD1 2CD3 2CD5 2CD7 2CD9 2CDB 2CDD 2CDF 2CE1 2CE3 2CEC 2CEE 2CF3 2D00-2D25 2D27 2D2D A641 \
A643 A645 A647 A649 A64B A64D A64F A651 A653 A655 A657 A659 A65B A65D A65F A661 A663 A665 A667 A669 A66B A66D \
A681 A683 A685 A687 A689 A68B A68D A68F A691 A693 A695 A697 A699 A69B A723 A725 A727 A729 A72B A72D A72F A733 \
A735 A737 A739 A73B A73D A73F A741 A743 A745 A747 A749 A74B A74D A74F A751 A753 A755 A757 A759 A75B A75D A75F \
A761 A763 A765 A767 A769 A76B A76D A76F A77A A77C A77F A781 A783 A785 A787 A78C A791 A793-A794 A797 A799 A79B \
A79D A79F A7A1 A7A3 A7A5 A7A7 A7A9 A7B5 A7B7 A7B9 A7BB A7BD A7BF A7C1 A7C3 A7C8 A7CA A7CD A7D1 A7D7 A7D9 A7DB \
A7F6 AB53 AB70-ABBF FB00-FB06 FB13-FB17 FF41-FF5A 10428-1044F 104D8-104FB 10597-105A1 105A3-105B1 105B3-105B9 \
105BB-105BC 10CC0-10CF2 10D70-10D85 118C0-118DF 16E60-16E7F 1E922-1E943";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_string_semantics() {
        assert_eq!(strip("\u{1c} a b\u{a0}\n"), "a b");
        assert_eq!(split("  a\u{1f}b\t c "), vec!["a", "b", "c"]);
        assert_eq!(splitlines("a\r\nb\rc\u{2028}d\n", false), vec!["a", "b", "c", "d"]);
        assert_eq!(splitlines("a\r\nb", true), vec!["a\r\n", "b"]);
        assert_eq!(splitlines("", false), Vec::<&str>::new());
        assert_eq!(splitlines("\n", false), vec![""]);
        assert_eq!(head("héllo", 2), "hé");
        assert_eq!(tail("héllo", 3), "llo");
        assert_eq!(tail("ab", 0), "ab");
        assert_eq!(slice("héllo", 1, 3), "él");
        assert_eq!(repr("it's"), "\"it's\"");
        assert_eq!(repr("a'\"\n\x7f"), "'a\\'\"\\n\\x7f'");
    }

    #[test]
    fn round_is_pythons_round() {
        for (x, want) in [(0.25, 0.2), (0.35, 0.3), (0.05, 0.1), (1.15, 1.1), (2.5, 2.5), (12.34, 12.3), (0.0, 0.0), (9.96, 10.0)] {
            assert_eq!(round(x, 1), want, "{x}");
        }
        assert_eq!(round(0.0078125, 6), 0.007812);
    }

    #[test]
    fn paths_normalise_as_pathlib_and_posixpath() {
        for (raw, want) in [("a//b/./c/", "a/b/c"), ("//a", "//a"), ("///a", "/a"), ("", "."), ("./", "."), ("/", "/"), ("//", "//"),
                            ("//a/./b/", "//a/b"), ("a/../b", "a/../b")] {
            assert_eq!(bytes_of(&path(Path::new(raw))), want.as_bytes(), "{raw}");
        }
        assert_eq!(normpath(b"/a/../../b/./c/.."), b"/b");
        assert_eq!(normpath(b"../a/.."), b"..");
        #[cfg(unix)]
        assert_eq!(bytes_of(&join(Path::new("."), "auth.json")), b"auth.json");
        for (p, want) in [("auth.json", "."), ("/auth.json", "/"), ("//auth.json", "//"), ("a/b", "a")] {
            assert_eq!(bytes_of(&parent(Path::new(p))), want.as_bytes(), "{p}");
        }
        assert_eq!(path_name("/usr/bin/"), "bin");
        assert_eq!(path_name(".."), "..");
        assert_eq!(path_name("a/."), "a");
        assert_eq!(path_name("/"), "");
    }
}
