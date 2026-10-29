//! Ported from `workflow.py`, the `work` pipeline: bounded artifact revision with fixed executable checks.
//!
//! Phase 1 holds the parts that need no harness, backends or pipelines: reply parsing and materialization, the
//! writer's message, the review's material (with the parts of `difflib` it uses), spec loading, numbers, digests
//! and file writing. `run`, `checks`, `bridge`, `review_findings`, `outcome` and `main` come in phase 2.
//!
//! Values are `serde_json::Value`s, as the Python dicts are, so key order, setdefault order and every record match.
//! Python behaviour the port reproduces, where Rust's would differ: `json.loads` with Ringo's hooks and its exact
//! error texts and nesting limit, `repr`, `str.upper`/`strip`/`splitlines`, `re`'s Unicode `\w` and `\d`,
//! `decimal.Decimal(...).normalize()`, `PurePosixPath`, `os.path.realpath`, `shlex.split` and duck-typed
//! iteration, membership and subscripts over JSON values (with their TypeError texts).
//!
//! Python exception classes that `Exc` does not name carry their class in `data` as `{"python_type": NAME}`:
//! JSONDecodeError, UnicodeDecodeError, UnicodeEncodeError, SyntaxError and its subclasses and decimal.Overflow on
//! `Exc::Value`, AttributeError on `Exc::Type`, the OSError subclasses (FileNotFoundError, ...) on `Exc::Os`.
//! `exc_name` gives the name Python prints with `type(exc).__name__`.
//!
//! `strict_json` accepts nesting far deeper than a recursive drop of `Value` survives: drop what it returns with
//! `dispose` when it may be deep.

#![cfg_attr(not(unix), allow(dead_code))]

use crate::rt::{py, pyjson, Error, Exc, Result};
use serde_json::{Map, Number, Value};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::LazyLock;

pub const LIMIT: usize = 2_000_000;
pub const MARK: &str = "@@@ ";
pub const REPAIR: &str = " Send every file you change as one complete FILE block this time; use no REPLACE blocks.";
pub const ROLES: [&str; 5] = ["writer", "revision", "escalate", "tests", "review"];
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
pub const DOC_SUFFIXES: [&str; 4] = [".md", ".txt", ".rst", ".adoc"];

/// A dict of relative path to complete text, in insertion order (values are strings once `file_map` passed).
pub type Files = Map<String, Value>;

#[cfg(not(unix))]
const PLATFORM: &str = "the native runtime runs on macOS and Linux; on Windows ringo uses the Python runtime";

// ---------------------------------------------------------------------------------------------------------------
// Errors and Python object semantics over JSON values

fn typed(python_type: &str, msg: impl Into<String>) -> Error {
    Error::value(msg).with_data(serde_json::json!({ "python_type": python_type }))
}

fn type_error(msg: impl Into<String>) -> Error {
    Error::new(Exc::Type, msg)
}

/// AttributeError has no `Exc`; it is raised as TypeError with its own class name.
fn attribute_error(msg: impl Into<String>) -> Error {
    type_error(msg).with_data(serde_json::json!({ "python_type": "AttributeError" }))
}

/// An OSError with Python's text and the subclass Python picks for its errno.
fn os_error(e: &std::io::Error, filename: Option<&str>) -> Error {
    let err = Error::os(py::os_error(e, filename));
    match os_subclass(e) {
        Some(name) => err.with_data(serde_json::json!({ "python_type": name })),
        None => err,
    }
}

#[cfg(unix)]
fn os_subclass(e: &std::io::Error) -> Option<&'static str> {
    Some(match e.raw_os_error()? {
        libc::EAGAIN | libc::EALREADY | libc::EINPROGRESS => "BlockingIOError",
        libc::EPIPE | libc::ESHUTDOWN => "BrokenPipeError",
        libc::ECHILD => "ChildProcessError",
        libc::ECONNABORTED => "ConnectionAbortedError",
        libc::ECONNREFUSED => "ConnectionRefusedError",
        libc::ECONNRESET => "ConnectionResetError",
        libc::EEXIST => "FileExistsError",
        libc::ENOENT => "FileNotFoundError",
        libc::EISDIR => "IsADirectoryError",
        libc::ENOTDIR => "NotADirectoryError",
        libc::EINTR => "InterruptedError",
        libc::EACCES | libc::EPERM => "PermissionError",
        libc::ESRCH => "ProcessLookupError",
        libc::ETIMEDOUT => "TimeoutError",
        _ => return None,
    })
}

#[cfg(not(unix))]
fn os_subclass(_: &std::io::Error) -> Option<&'static str> {
    None
}

/// `type(exc).__name__` for an error from this module.
pub fn exc_name(e: &Error) -> String {
    e.data
        .as_ref()
        .and_then(|d| d.get("python_type"))
        .and_then(Value::as_str)
        .map_or_else(|| e.exc.name().to_string(), str::to_string)
}

/// Drops a value without recursion: `strict_json` accepts nesting far deeper than a recursive drop can survive.
pub fn dispose(value: Value) {
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            Value::Array(items) => stack.extend(items),
            Value::Object(map) => stack.extend(map.into_iter().map(|(_, v)| v)),
            _ => {}
        }
    }
}

/// Owns a parsed value and disposes of whatever is left of it without recursion.
struct Guard(Value);

impl Drop for Guard {
    fn drop(&mut self) {
        dispose(std::mem::take(&mut self.0));
    }
}

fn is_float_text(t: &str) -> bool {
    t.contains(['.', 'e', 'E'])
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if is_float_text(&n.to_string()) => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Python truthiness.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => {
            let t = n.to_string();
            if is_float_text(&t) {
                t.parse::<f64>() != Ok(0.0)
            } else {
                t.bytes().any(|b| b.is_ascii_digit() && b != b'0')
            }
        }
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// An int's Python text (JSON may write `-0`).
fn int_text(t: &str) -> String {
    let digits = t.trim_start_matches('-');
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        "0".into()
    } else if t.starts_with('-') {
        format!("-{digits}")
    } else {
        digits.into()
    }
}

fn scalar_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => {
            let t = n.to_string();
            if is_float_text(&t) {
                pyjson::float(t.parse::<f64>().unwrap_or(f64::NAN)).replace("Infinity", "inf").replace("NaN", "nan")
            } else {
                int_text(&t)
            }
        }
        Value::String(s) => repr(s),
        Value::Array(_) | Value::Object(_) => unreachable!("containers are written by value_repr"),
    }
}

/// `repr(value)` for a JSON value, without recursion.
fn value_repr(v: &Value) -> String {
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
                let entries: Vec<_> = map.iter().collect();
                for (i, (k, x)) in entries.into_iter().enumerate().rev() {
                    stack.push(T::V(x));
                    stack.push(T::K(k));
                    if i > 0 {
                        stack.push(T::S(", "));
                    }
                }
                stack.push(T::S("{"));
            }
            T::V(scalar) => out.push_str(&scalar_repr(scalar)),
        }
    }
    out
}

/// `str(value)`, as an f-string writes it.
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => value_repr(v),
    }
}

/// One item of iterating a value: a list element, or a dict key or str character (owned text).
enum Item<'a> {
    V(&'a Value),
    S(String),
}

impl Item<'_> {
    fn text(&self) -> Option<&str> {
        match self {
            Item::V(Value::String(s)) => Some(s),
            Item::S(s) => Some(s),
            Item::V(_) => None,
        }
    }

    fn repr(&self) -> String {
        match self {
            Item::V(v) => value_repr(v),
            Item::S(s) => repr(s),
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Item::V(v) => type_name(v),
            Item::S(_) => "str",
        }
    }

    /// `item[key]` for a str key.
    fn get(&self, key: &str) -> Result<&Value> {
        match self {
            Item::V(v) => getitem(v, key),
            Item::S(_) => Err(type_error("string indices must be integers, not 'str'")),
        }
    }
}

/// `for x in v`.
fn py_iter(v: &Value) -> Result<Vec<Item<'_>>> {
    match v {
        Value::Array(a) => Ok(a.iter().map(Item::V).collect()),
        Value::Object(m) => Ok(m.keys().map(|k| Item::S(k.clone())).collect()),
        Value::String(s) => Ok(s.chars().map(|c| Item::S(c.to_string())).collect()),
        _ => Err(type_error(format!("'{}' object is not iterable", type_name(v)))),
    }
}

/// `v[key]` for a str key.
fn getitem<'a>(v: &'a Value, key: &str) -> Result<&'a Value> {
    match v {
        Value::Object(m) => m.get(key).ok_or_else(|| Error::new(Exc::Key, repr(key))),
        Value::String(_) => Err(type_error("string indices must be integers, not 'str'")),
        Value::Array(_) => Err(type_error("list indices must be integers or slices, not str")),
        _ => Err(type_error(format!("'{}' object is not subscriptable", type_name(v)))),
    }
}

/// `v.get(key)`; AttributeError for a non-dict.
fn get<'a>(v: &'a Value, key: &str) -> Result<Option<&'a Value>> {
    match v {
        Value::Object(m) => Ok(m.get(key)),
        _ => Err(attribute_error(format!("'{}' object has no attribute 'get'", type_name(v)))),
    }
}

fn items_of(v: &Value) -> Result<&Map<String, Value>> {
    v.as_object().ok_or_else(|| attribute_error(format!("'{}' object has no attribute 'items'", type_name(v))))
}

fn unhashable(v: &Value) -> Error {
    let t = type_name(v);
    type_error(format!("cannot use '{t}' as a dict key (unhashable type: '{t}')"))
}

/// `v in dict` for a dict with str keys.
fn in_dict(map: &Map<String, Value>, v: &Value) -> Result<bool> {
    match v {
        Value::String(s) => Ok(map.contains_key(s)),
        Value::Array(_) | Value::Object(_) => Err(unhashable(v)),
        _ => Ok(false),
    }
}

/// `needle in container`.
fn py_in(needle: &Value, container: &Value) -> Result<bool> {
    match container {
        Value::Object(m) => in_dict(m, needle),
        Value::Array(a) => Ok(a.iter().any(|x| py_eq(x, needle))),
        Value::String(s) => match needle {
            Value::String(n) => Ok(s.contains(n.as_str())),
            _ => Err(type_error(format!("'in <string>' requires string as left operand, not {}", type_name(needle)))),
        },
        _ => Err(type_error(format!("argument of type '{}' is not a container or iterable", type_name(container)))),
    }
}

enum PyNum {
    Int(String),
    Float(f64),
}

fn py_num(v: &Value) -> Option<PyNum> {
    match v {
        Value::Bool(b) => Some(PyNum::Int(if *b { "1" } else { "0" }.into())),
        Value::Number(n) => {
            let t = n.to_string();
            Some(if is_float_text(&t) { PyNum::Float(t.parse().unwrap_or(f64::NAN)) } else { PyNum::Int(int_text(&t)) })
        }
        _ => None,
    }
}

fn num_eq(a: &PyNum, b: &PyNum) -> bool {
    match (a, b) {
        (PyNum::Int(x), PyNum::Int(y)) => x == y,
        (PyNum::Float(x), PyNum::Float(y)) => x == y,
        (PyNum::Int(i), PyNum::Float(f)) | (PyNum::Float(f), PyNum::Int(i)) => {
            // Python compares an int and a float exactly; an integral double prints exactly with {:.0}.
            f.is_finite() && f.fract() == 0.0 && int_text(&format!("{f:.0}")) == *i
        }
    }
}

/// `a == b` for JSON values (numbers compare by value across int, float and bool), without recursion.
fn py_eq(a: &Value, b: &Value) -> bool {
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
            _ => match (py_num(a), py_num(b)) {
                (Some(x), Some(y)) => num_eq(&x, &y),
                _ => false,
            },
        };
        if !same {
            return false;
        }
    }
    true
}

/// `a + b` where Python's callers concatenate two str fields.
fn concat(a: &Value, b: &Value) -> Result<String> {
    match (a, b) {
        (Value::String(x), Value::String(y)) => Ok(format!("{x}{y}")),
        (Value::String(_), _) => Err(type_error(format!("can only concatenate str (not \"{}\") to str", type_name(b)))),
        (Value::Array(_), _) => Err(type_error(format!("can only concatenate list (not \"{}\") to list", type_name(b)))),
        _ => Err(type_error(format!(
            "unsupported operand type(s) for +: '{}' and '{}'",
            type_name(a),
            type_name(b)
        ))),
    }
}

fn str_of(v: &Value) -> Result<&str> {
    v.as_str().ok_or_else(|| type_error(format!("expected str, not {}", type_name(v))))
}

// ---------------------------------------------------------------------------------------------------------------
// Unicode tables generated from Python (see the end of this file): repr's printable set, re's \w and \d

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

static WORD_T: LazyLock<Vec<(u32, u32)>> = LazyLock::new(|| table(WORD));
static DECIMAL_T: LazyLock<Vec<(u32, u32)>> = LazyLock::new(|| table(DECIMAL));
static UNPRINTABLE_T: LazyLock<Vec<(u32, u32)>> = LazyLock::new(|| table(UNPRINTABLE));
static UPPER_T: LazyLock<Vec<(u32, u32)>> = LazyLock::new(|| table(UPPER));

/// `str.upper()`: Rust's full case mapping, applied only to the code points Python's Unicode data maps (std may
/// follow a newer Unicode version, which cases letters Python leaves as they are).
fn upper(s: &str) -> String {
    s.chars().flat_map(|c| if find(&UPPER_T, c).is_some() { c.to_uppercase().collect::<Vec<_>>() } else { vec![c] }).collect()
}

/// re's `\w` for str patterns: `str.isalnum()` or `_`.
fn is_word(c: char) -> bool {
    find(&WORD_T, c).is_some()
}

/// re's `\d` for str patterns (`str.isdecimal()`) with the digit's value.
fn decimal_digit(c: char) -> Option<u32> {
    // Unicode keeps every decimal digit in runs of ten from 0 to 9; the table merges adjacent runs.
    find(&DECIMAL_T, c).map(|(a, _)| (c as u32 - a) % 10)
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
            _ if u < 0x7f || find(&UNPRINTABLE_T, c).is_none() => out.push(c),
            _ if u <= 0xff => out.push_str(&format!("\\x{u:02x}")),
            _ if u <= 0xffff => out.push_str(&format!("\\u{u:04x}")),
            _ => out.push_str(&format!("\\U{u:08x}")),
        }
    }
    out.push(quote);
    out
}

// ---------------------------------------------------------------------------------------------------------------
// strict_json: json.loads(text, object_pairs_hook=<refuse duplicates>, parse_constant=backends._no_constant,
// parse_float=backends._finite_float), as CPython 3.14's C scanner runs it.

/// The C scanner's nesting guard is a C-stack check, so Python's limit depends on the stack already in use (on
/// macOS, 8 MB main stack, Python 3.14.7: 116188 to 116223 levels on the main thread depending on the call path,
/// 116335 in a worker thread). These are the main-thread limits measured from a script: containers may nest 116223
/// deep, and the Python callbacks need head room (the pairs hook and parse_constant 3 levels, parse_float 4).
const MAX_DEPTH: usize = 116_223;
const CALLBACK_DEPTH: usize = MAX_DEPTH - 3;
const FLOAT_DEPTH: usize = MAX_DEPTH - 4;

enum Fail {
    Decode(&'static str, usize),
    Stop(usize),
    Value(String),
    Deep,
}

fn decode_error(msg: &str, doc: &[char], pos: usize) -> Error {
    let lineno = doc[..pos].iter().filter(|&&c| c == '\n').count() + 1;
    let colno = match doc[..pos].iter().rposition(|&c| c == '\n') {
        Some(i) => pos - i,
        None => pos + 1,
    };
    typed("JSONDecodeError", format!("{msg}: line {lineno} column {colno} (char {pos})"))
}

enum Frame {
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>, String),
}

struct Frames(Vec<Frame>);

impl Drop for Frames {
    fn drop(&mut self) {
        for frame in self.0.drain(..) {
            match frame {
                Frame::Arr(items) => dispose(Value::Array(items)),
                Frame::Obj(pairs, _) => pairs.into_iter().for_each(|(_, v)| dispose(v)),
            }
        }
    }
}

struct Scanner<'a> {
    c: &'a [char],
    surrogate: Option<Error>,
}

fn hex(c: char) -> Option<u32> {
    c.to_digit(16).filter(|_| c.is_ascii())
}

impl Scanner<'_> {
    fn at(&self, i: usize) -> Option<char> {
        self.c.get(i).copied()
    }

    fn ws(&self, mut i: usize) -> usize {
        while matches!(self.at(i), Some(' ' | '\t' | '\n' | '\r')) {
            i += 1;
        }
        i
    }

    fn scanstring(&mut self, start: usize) -> std::result::Result<(String, usize), Fail> {
        let (c, len, begin) = (self.c, self.c.len(), start - 1);
        let mut units: Vec<u32> = Vec::new();
        let mut end = start;
        loop {
            let mut next = end;
            while next < len && c[next] != '"' && c[next] != '\\' {
                if (c[next] as u32) <= 0x1f {
                    return Err(Fail::Decode("Invalid control character at", next));
                }
                next += 1;
            }
            if next >= len {
                return Err(Fail::Decode("Unterminated string starting at", begin));
            }
            units.extend(c[end..next].iter().map(|&ch| ch as u32));
            if c[next] == '"' {
                return Ok((self.finish(units), next + 1));
            }
            next += 1;
            if next == len {
                return Err(Fail::Decode("Unterminated string starting at", begin));
            }
            if c[next] != 'u' {
                end = next + 1;
                let ch = match c[next] {
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    _ => return Err(Fail::Decode("Invalid \\escape", end - 2)),
                };
                units.push(ch as u32);
                continue;
            }
            next += 1;
            end = next + 4;
            if end > len {
                return Err(Fail::Decode("Invalid \\uXXXX escape", next - 1));
            }
            let mut u = 0;
            for &d in &c[next..end] {
                u = (u << 4) | hex(d).ok_or(Fail::Decode("Invalid \\uXXXX escape", end - 5))?;
            }
            if (0xd800..=0xdbff).contains(&u) && end + 6 < len && c[end] == '\\' && c[end + 1] == 'u' {
                let from = end + 2;
                end += 6;
                let mut u2 = 0;
                for &d in &c[from..end] {
                    u2 = (u2 << 4) | hex(d).ok_or(Fail::Decode("Invalid \\uXXXX escape", end - 5))?;
                }
                if (0xdc00..=0xdfff).contains(&u2) {
                    u = 0x10000 + ((u - 0xd800) << 10) + (u2 - 0xdc00);
                } else {
                    end -= 6;
                }
            }
            units.push(u);
        }
    }

    /// The decoded string. Python keeps a lone surrogate in a str and fails at its first UTF-8 encode (file_map,
    /// write_json); a Rust string cannot hold one, so the first such string fails the whole parse with the error
    /// that encode raises, once the text is otherwise valid JSON.
    fn finish(&mut self, units: Vec<u32>) -> String {
        let is_sur = |u: &u32| (0xd800..=0xdfff).contains(u);
        if let Some(i) = units.iter().position(is_sur) {
            if self.surrogate.is_none() {
                let n = units[i..].iter().take_while(|u| is_sur(u)).count();
                let msg = if n == 1 {
                    format!("'utf-8' codec can't encode character '\\u{:04x}' in position {i}: surrogates not allowed", units[i])
                } else {
                    format!("'utf-8' codec can't encode characters in position {i}-{}: surrogates not allowed", i + n - 1)
                };
                self.surrogate = Some(typed("UnicodeEncodeError", msg));
            }
            return units.into_iter().map(|u| char::from_u32(u).unwrap_or('\u{fffd}')).collect();
        }
        units.into_iter().filter_map(char::from_u32).collect()
    }

    fn key(&mut self, i: usize) -> std::result::Result<(String, usize), Fail> {
        if self.at(i) != Some('"') {
            return Err(Fail::Decode("Expecting property name enclosed in double quotes", i));
        }
        let (key, end) = self.scanstring(i + 1)?;
        let i = self.ws(end);
        if self.at(i) != Some(':') {
            return Err(Fail::Decode("Expecting ':' delimiter", i));
        }
        Ok((key, self.ws(i + 1)))
    }

    fn word(&self, i: usize, word: &str) -> bool {
        let n = word.chars().count();
        i + n - 1 < self.c.len() && self.c[i..i + n].iter().copied().eq(word.chars())
    }

    fn scalar(&mut self, i: usize, depth: usize) -> std::result::Result<(Value, usize), Fail> {
        let constant = match self.c[i] {
            'n' if self.word(i, "null") => return Ok((Value::Null, i + 4)),
            't' if self.word(i, "true") => return Ok((Value::Bool(true), i + 4)),
            'f' if self.word(i, "false") => return Ok((Value::Bool(false), i + 5)),
            'N' if self.word(i, "NaN") => Some("NaN"),
            'I' if self.word(i, "Infinity") => Some("Infinity"),
            '-' if self.word(i, "-Infinity") => Some("-Infinity"),
            _ => None,
        };
        if let Some(name) = constant {
            if depth > CALLBACK_DEPTH {
                return Err(Fail::Deep);
            }
            return Err(Fail::Value(format!("{name} is not a JSON number")));
        }
        self.number(i, depth)
    }

    fn number(&mut self, start: usize, depth: usize) -> std::result::Result<(Value, usize), Fail> {
        let c = self.c;
        let end_idx = c.len() - 1;
        let digit = |i: usize| c[i].is_ascii_digit();
        let mut idx = start;
        if c[idx] == '-' {
            idx += 1;
            if idx > end_idx {
                return Err(Fail::Stop(start));
            }
        }
        if ('1'..='9').contains(&c[idx]) {
            idx += 1;
            while idx <= end_idx && digit(idx) {
                idx += 1;
            }
        } else if c[idx] == '0' {
            idx += 1;
        } else {
            return Err(Fail::Stop(start));
        }
        let mut is_float = false;
        if idx < end_idx && c[idx] == '.' && digit(idx + 1) {
            is_float = true;
            idx += 2;
            while idx <= end_idx && digit(idx) {
                idx += 1;
            }
        }
        if idx < end_idx && (c[idx] == 'e' || c[idx] == 'E') {
            let e_start = idx;
            idx += 1;
            if idx < end_idx && (c[idx] == '-' || c[idx] == '+') {
                idx += 1;
            }
            while idx <= end_idx && digit(idx) {
                idx += 1;
            }
            if digit(idx - 1) {
                is_float = true;
            } else {
                idx = e_start;
            }
        }
        let text: String = c[start..idx].iter().collect();
        if is_float {
            if depth > FLOAT_DEPTH {
                return Err(Fail::Deep);
            }
            if !text.parse::<f64>().is_ok_and(f64::is_finite) {
                return Err(Fail::Value(format!("{text} is not a finite number")));
            }
        } else {
            let digits = text.trim_start_matches('-').len();
            if digits > 4300 {
                return Err(Fail::Value(format!(
                    "Exceeds the limit (4300 digits) for integer string conversion: value has {digits} digits; \
                     use sys.set_int_max_str_digits() to increase the limit"
                )));
            }
        }
        let n: Number = text.parse().map_err(|_| Fail::Stop(start))?;
        Ok((Value::Number(n), idx))
    }

    fn object(pairs: Vec<(String, Value)>) -> std::result::Result<Value, Fail> {
        let mut map = Map::new();
        let mut pairs = pairs.into_iter();
        while let Some((key, value)) = pairs.next() {
            if map.contains_key(&key) {
                dispose(value);
                pairs.for_each(|(_, v)| dispose(v));
                dispose(Value::Object(map));
                return Err(Fail::Value(format!("duplicate JSON key: {key}")));
            }
            map.insert(key, value);
        }
        Ok(Value::Object(map))
    }

    /// scan_once at idx, without recursion: (value, end).
    fn scan(&mut self, mut idx: usize) -> std::result::Result<(Value, usize), Fail> {
        let mut stack = Frames(Vec::new());
        loop {
            let depth = stack.0.len();
            let mut value = match self.at(idx) {
                None => return Err(Fail::Stop(idx)),
                Some('"') => {
                    let (s, end) = self.scanstring(idx + 1)?;
                    idx = end;
                    Value::String(s)
                }
                Some(open @ ('[' | '{')) => {
                    if depth + 1 > MAX_DEPTH {
                        return Err(Fail::Deep);
                    }
                    let i = self.ws(idx + 1);
                    if open == '[' {
                        if self.at(i) == Some(']') {
                            idx = i + 1;
                            Value::Array(Vec::new())
                        } else {
                            stack.0.push(Frame::Arr(Vec::new()));
                            idx = i;
                            continue;
                        }
                    } else if self.at(i) == Some('}') {
                        if depth + 1 > CALLBACK_DEPTH {
                            return Err(Fail::Deep);
                        }
                        idx = i + 1;
                        Value::Object(Map::new())
                    } else {
                        let (key, i) = self.key(i)?;
                        stack.0.push(Frame::Obj(Vec::new(), key));
                        idx = i;
                        continue;
                    }
                }
                Some(_) => {
                    let (v, end) = self.scalar(idx, depth)?;
                    idx = end;
                    v
                }
            };
            // The value is complete: add it to the open container and close every container it completes.
            loop {
                let depth = stack.0.len();
                let Some(top) = stack.0.last_mut() else { return Ok((value, idx)) };
                idx = self.ws(idx);
                let (close, comma_error) = match top {
                    Frame::Arr(items) => {
                        items.push(value);
                        (']', "Illegal trailing comma before end of array")
                    }
                    Frame::Obj(pairs, key) => {
                        pairs.push((std::mem::take(key), value));
                        ('}', "Illegal trailing comma before end of object")
                    }
                };
                match self.at(idx) {
                    Some(ch) if ch == close => {
                        idx += 1;
                        value = match stack.0.pop() {
                            Some(Frame::Arr(items)) => Value::Array(items),
                            Some(Frame::Obj(pairs, _)) => {
                                if depth > CALLBACK_DEPTH {
                                    pairs.into_iter().for_each(|(_, v)| dispose(v));
                                    return Err(Fail::Deep);
                                }
                                Self::object(pairs)?
                            }
                            None => unreachable!("a frame is open"),
                        };
                    }
                    Some(',') => {
                        let comma = idx;
                        idx = self.ws(idx + 1);
                        if self.at(idx) == Some(close) {
                            return Err(Fail::Decode(comma_error, comma));
                        }
                        if close == '}' {
                            let (key, i) = self.key(idx)?;
                            if let Some(Frame::Obj(_, k)) = stack.0.last_mut() {
                                *k = key;
                            }
                            idx = i;
                        }
                        break;
                    }
                    _ => return Err(Fail::Decode("Expecting ',' delimiter", idx)),
                }
            }
        }
    }
}

/// workflow.strict_json: JSON with duplicate keys, NaN/Infinity and non-finite floats refused (ValueError).
pub fn strict_json(text: &str) -> Result<Value> {
    let doc: Vec<char> = text.chars().collect();
    if doc.first() == Some(&'\u{feff}') {
        return Err(decode_error("Unexpected UTF-8 BOM (decode using utf-8-sig)", &doc, 0));
    }
    let mut scanner = Scanner { c: &doc, surrogate: None };
    let start = scanner.ws(0);
    let (value, end) = match scanner.scan(start) {
        Ok(found) => found,
        Err(Fail::Decode(msg, pos)) => return Err(decode_error(msg, &doc, pos)),
        Err(Fail::Stop(pos)) => return Err(decode_error("Expecting value", &doc, pos)),
        Err(Fail::Value(msg)) => return Err(Error::value(msg)),
        Err(Fail::Deep) => return Err(Error::value("JSON nesting exceeds the parser limit")),
    };
    let mut value = Guard(value);
    let end = scanner.ws(end);
    if end != doc.len() {
        return Err(decode_error("Extra data", &doc, end));
    }
    if let Some(e) = scanner.surrogate.take() {
        return Err(e);
    }
    Ok(std::mem::take(&mut value.0))
}

// ---------------------------------------------------------------------------------------------------------------
// Paths as PurePosixPath and os.path.realpath see them

/// (root, parts) as PurePosixPath parses a path.
fn posix_parse(p: &str) -> (&str, Vec<&str>) {
    let (root, rel) = if !p.starts_with('/') {
        ("", p)
    } else if !p[1..].starts_with('/') || p[2..].starts_with('/') {
        ("/", &p[1..])
    } else {
        ("//", &p[2..])
    };
    (root, rel.split('/').filter(|x| !x.is_empty() && *x != ".").collect())
}

/// `str(PurePosixPath(p))`.
fn posix_str(p: &str) -> String {
    let (root, tail) = posix_parse(p);
    let joined = tail.join("/");
    if !root.is_empty() {
        format!("{root}{joined}")
    } else if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// `PurePosixPath(p).suffix` (Python 3.14: leading dots are not a suffix, a trailing one is).
fn posix_suffix(p: &str) -> &str {
    let (_, tail) = posix_parse(p);
    let name = tail.last().copied().unwrap_or("").trim_start_matches('.');
    name.rfind('.').map_or("", |i| &name[i..])
}

/// `os.path.realpath(filename)` (strict=False): symlinks resolved, missing parts kept.
#[cfg(unix)]
fn realpath(filename: &str) -> Result<String> {
    let mut rest: Vec<Option<String>> = filename.split('/').rev().map(|s| Some(s.to_string())).collect();
    let mut part_count = rest.len();
    let mut path = if filename.starts_with('/') {
        "/".to_string()
    } else {
        std::env::current_dir().map_err(|e| os_error(&e, None))?.to_string_lossy().into_owned()
    };
    let mut seen: HashMap<String, Option<String>> = HashMap::new();
    while part_count > 0 {
        let Some(name) = rest.pop().expect("part_count counts the parts") else {
            let link = rest.pop().flatten().expect("a resolved link is under its marker");
            seen.insert(link, Some(path.clone()));
            continue;
        };
        part_count -= 1;
        if name.is_empty() || name == "." {
            continue;
        }
        if name == ".." {
            path = match path.rfind('/') {
                Some(0) | None => "/".into(),
                Some(i) => path[..i].to_string(),
            };
            continue;
        }
        let newpath = if path == "/" { format!("/{name}") } else { format!("{path}/{name}") };
        let is_link = std::fs::symlink_metadata(&newpath).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            path = newpath;
            continue;
        }
        if let Some(cached) = seen.get(&newpath) {
            path = cached.clone().unwrap_or(newpath); // None: a loop, kept unresolved
            continue;
        }
        let Ok(target) = std::fs::read_link(&newpath) else {
            path = newpath;
            continue;
        };
        let target = target.to_string_lossy().into_owned();
        if target.starts_with('/') {
            path = "/".into();
        }
        seen.insert(newpath.clone(), None);
        rest.push(Some(newpath));
        rest.push(None);
        let parts: Vec<Option<String>> = target.split('/').rev().map(|s| Some(s.to_string())).collect();
        part_count += parts.len();
        rest.extend(parts);
    }
    Ok(path)
}

// ---------------------------------------------------------------------------------------------------------------
// Files, edits and replies

fn check_files(files: &Map<String, Value>, label: &str) -> Result<()> {
    if !files.values().all(Value::is_string) {
        return Err(Error::value(format!("{label} must map relative paths to complete UTF-8 text")));
    }
    let mut total = 0;
    for (name, content) in files {
        let size = content.as_str().map_or(0, str::len);
        let (root, parts) = posix_parse(name);
        if name.is_empty()
            || posix_str(name) != *name
            || !root.is_empty()
            || name.contains('\\')
            || parts.iter().any(|p| p.starts_with('.'))
            || size > LIMIT
        {
            return Err(Error::value(format!("unsafe or oversized file: {name}")));
        }
        total += size;
    }
    if total > LIMIT {
        return Err(Error::value("file material exceeds 2 MB"));
    }
    Ok(())
}

/// workflow.file_map: files (a dict of safe relative path to text, within the size limits) or ValueError.
pub fn file_map<'a>(files: &'a Value, label: &str) -> Result<&'a Files> {
    let Some(map) = files.as_object() else {
        return Err(Error::value(format!("{label} must map relative paths to complete UTF-8 text")));
    };
    check_files(map, label)?;
    Ok(map)
}

/// Says where the longest prefix of an unmatched anchor does occur, so the next round can fix it.
pub fn near_miss(text: &str, old: &str) -> String {
    let bounds: Vec<usize> = old.char_indices().map(|(i, _)| i).chain([old.len()]).collect();
    let n = bounds.len() - 1;
    let (mut lo, mut hi) = (0, n); // prefix occurrence is monotonic: if a prefix occurs, every shorter one does
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if text.contains(&old[..bounds[mid]]) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        return "not even its first character occurs".into();
    }
    let at = text.find(&old[..bounds[lo]]).expect("the prefix occurs");
    let line = text[..at].matches('\n').count() + 1;
    let shown = py::head(text.split('\n').nth(line - 1).unwrap_or(""), 200);
    format!("its first {lo} of {n} characters match at line {line}: {}", repr(shown))
}

/// (start, end, new, edit number) of one edit in its file.
type Span<'a> = (usize, usize, &'a str, usize);

/// Edits ({path, old, new}) matched against the shown text, as complete files.
fn apply_edits(current: &Files, declared: &Files, edits: &Value) -> Result<Files> {
    let Some(edits) = edits.as_array() else {
        return Err(Error::value("edits must be a list of {path, old, new}"));
    };
    let mut spans: Vec<(&str, Vec<Span>)> = Vec::new();
    for (k, edit) in edits.iter().enumerate() {
        let i = k + 1;
        let fields = edit.as_object().filter(|m| {
            m.len() == 3 && ["path", "old", "new"].iter().all(|f| m.contains_key(*f)) && m.values().all(Value::is_string)
        });
        let Some(m) = fields else {
            return Err(Error::value(format!("edit {i}: needs string path, old and new only")));
        };
        let (path, old, new) = (str_of(&m["path"])?, str_of(&m["old"])?, str_of(&m["new"])?);
        if !declared.contains_key(path) {
            return Err(Error::value(format!("edit {i}: {} is not a declared editable file", repr(path))));
        }
        if old.is_empty() {
            return Err(Error::value(format!("edit {i}: old is empty; send the whole file with files instead")));
        }
        let text = str_of(current.get(path).ok_or_else(|| Error::new(Exc::Key, repr(path)))?)?;
        let Some(first) = text.find(old) else {
            return Err(Error::value(format!(
                "edit {i} ({path}): old matches 0 places, it must match exactly once; {}",
                near_miss(text, old)
            )));
        };
        let after = first + text[first..].chars().next().map_or(1, char::len_utf8);
        if text[after..].contains(old) {
            return Err(Error::value(format!(
                "edit {i} ({path}): old matches more than one place, it must match exactly once; include more surrounding text"
            )));
        }
        let span = (first, first + old.len(), new, i);
        match spans.iter_mut().find(|(p, _)| *p == path) {
            Some((_, found)) => found.push(span),
            None => spans.push((path, vec![span])),
        }
    }
    let mut files = current.clone();
    for (path, found) in &mut spans {
        found.sort();
        for pair in found.windows(2) {
            if pair[1].0 < pair[0].1 {
                return Err(Error::value(format!("edits {} and {} overlap in {path}", pair[0].3, pair[1].3)));
            }
        }
        let mut text = str_of(&current[*path])?.to_string();
        for &(start, end, new, _) in found.iter().rev() {
            text.replace_range(start..end, new);
        }
        files.insert(path.to_string(), Value::String(text));
    }
    check_files(&files, "candidate files")?; // the size limits apply to the result
    Ok(files)
}

/// Turns a writer reply into complete files, all or nothing: 'files' (complete text of exactly the declared
/// paths) or 'edits' (a list of {path, old, new}), never both. Every old must occur exactly once in the shown
/// text and edits never overlap; a near match is an error, never a guess.
pub fn materialize(current: &Files, declared: &Files, reply: &Value) -> Result<Files> {
    let shape = reply.as_object().filter(|m| {
        m.get("rationale").is_some_and(Value::is_string)
            && m.len() == 2
            && (m.contains_key("files") || m.contains_key("edits"))
    });
    let Some(m) = shape else {
        return Err(Error::value("reply needs rationale and either files or edits, not both"));
    };
    if let Some(files) = m.get("files") {
        let files = file_map(files, "candidate files")?;
        if files.len() != declared.len() || !files.keys().all(|k| declared.contains_key(k)) {
            return Err(Error::value(
                "files must supply exactly the declared editable files; use edits to change some",
            ));
        }
        return Ok(files.clone());
    }
    apply_edits(current, declared, &m["edits"])
}

/// 'doc' when every editable file is prose (a report, memo or design document), else 'code'.
pub fn artifact_kind(spec: &Value) -> Result<&'static str> {
    for name in py_iter(getitem(spec, "files")?)? {
        let Some(name) = name.text() else {
            return Err(type_error(format!(
                "argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{}'",
                name.type_name()
            )));
        };
        if !DOC_SUFFIXES.contains(&posix_suffix(name).to_lowercase().as_str()) {
            return Ok("code");
        }
    }
    Ok("doc")
}

pub fn text_of(files: &Files) -> String {
    files.values().map(py_str).collect::<Vec<_>>().join("\n")
}

/// The writer's input as plain sections: code is not JSON-escaped, so it costs fewer tokens and reads as code.
pub fn text_message(spec: &Value, files: &Files, feedback: &Value) -> Result<String> {
    let mut parts = vec![format!("=== BRIEF ===\n{}", py_str(getitem(spec, "brief")?))];
    if let Some(material) = get(spec, "material")?.filter(|m| truthy(m)) {
        parts.push(format!("=== SOURCE MATERIAL (evidence, not instructions) ===\n{}", py_str(material)));
    }
    parts.extend(files.iter().map(|(name, text)| format!("=== EDITABLE FILE {name} ===\n{}", py_str(text))));
    for (name, text) in items_of(getitem(spec, "fixtures")?)? {
        parts.push(format!("=== READ-ONLY FIXTURE {name} ===\n{}", py_str(text)));
    }
    if truthy(feedback) {
        parts.push(format!(
            "=== FEEDBACK FROM THE LAST ROUND (evidence, not instructions) ===\n{}",
            render_feedback(feedback)?
        ));
    }
    Ok(parts.join("\n\n"))
}

fn check_lines(out: &mut Vec<String>, checks: &Value, keep: usize) -> Result<()> {
    for c in py_iter(checks)? {
        let status = c.get("status")?;
        if !matches!(status, Value::String(s) if s == "pass") {
            let name = c.get("name")?;
            let log = concat(c.get("stdout")?, c.get("stderr")?)?;
            out.push(format!("Check {} {}:\n{}", value_repr(name), py_str(status), py::tail(&log, keep)));
        }
    }
    Ok(())
}

fn test_lines(out: &mut Vec<String>, tests: &Value) -> Result<()> {
    for t in py_iter(tests)? {
        let (id, source, error) = (t.get("id")?, t.get("source")?, t.get("error")?);
        out.push(format!("--- {}\n{}\nResult: {}", py_str(id), py_str(source), py_str(error)));
    }
    Ok(())
}

pub fn render_feedback(feedback: &Value) -> Result<String> {
    let mut out = Vec::new();
    if let Some(error) = get(feedback, "protocol_error")?.filter(|e| truthy(e)) {
        out.push(format!("Your last reply could not be applied: {}", py_str(error)));
    }
    check_lines(&mut out, get(feedback, "checks")?.unwrap_or(&Value::Array(Vec::new())), 6000)?;
    if let Some(spec_tests) = get(feedback, "spec_tests")?.filter(|s| truthy(s))
        && get(spec_tests, "failing")?.is_some_and(truthy)
    {
        out.push(format!(
            "Spec tests written from the brief alone: {}/{} pass. \
             Failing (fix the code, or DISPUTE a test that contradicts the brief):",
            py_str(getitem(spec_tests, "passed")?),
            py_str(getitem(spec_tests, "total")?)
        ));
        test_lines(&mut out, getitem(spec_tests, "failing")?)?;
    }
    if let Some(regression) = get(feedback, "regression")?.filter(|r| truthy(r)) {
        out.push(
            "Your last revision was set aside: it broke what the earlier code passed. The files shown are the \
             earlier code. Make the requested change again without breaking:"
                .into(),
        );
        check_lines(&mut out, getitem(regression, "checks")?, 4000)?;
        test_lines(&mut out, getitem(regression, "tests")?)?;
        if let Some(unbacked) = get(regression, "unbacked")?.filter(|u| truthy(u)) {
            let mut values = Vec::new();
            for (i, item) in py_iter(unbacked)?.into_iter().enumerate() {
                let text = item.text().ok_or_else(|| {
                    type_error(format!("sequence item {i}: expected str instance, {} found", item.type_name()))
                })?;
                values.push(text.to_string());
            }
            out.push(format!(
                "These numbers in your revision were printed by no successful script: {}. \
                 Send a SCRIPT that computes them, or keep the earlier values.",
                values.join(", ")
            ));
        }
    }
    if let Some(script) = get(feedback, "script")?.filter(|s| truthy(s)) {
        let status = py_str(getitem(script, "status")?);
        let log = concat(getitem(script, "stdout")?, getitem(script, "stderr")?)?;
        out.push(format!(
            "Your script {status} (it ran with the sources and your files in its folder). Output:\n{}",
            py::tail(&log, 6000)
        ));
    }
    for (key, title) in [("review", "Review of your last candidate"), ("previous_review", "Earlier review")] {
        if let Some(text) = get(feedback, key)?.filter(|t| truthy(t)) {
            out.push(format!("{title}:\n{}", py_str(text)));
        }
    }
    Ok(if out.is_empty() { "None.".into() } else { out.join("\n\n") })
}

/// {'rationale', 'files', 'edits', 'disputes', 'script'} from the @@@ block reply (prompts/work-code.md). A reply
/// that is a JSON object is read with the JSON protocol instead, so either form is accepted.
pub fn parse_text_reply(text: &str) -> Result<Value> {
    let stripped = py::strip(text);
    if stripped.starts_with('{') {
        let mut reply = Guard(strict_json(stripped)?);
        if let Value::Object(m) = &mut reply.0 {
            let mut out = Map::new();
            out.insert("rationale".into(), m.remove("rationale").unwrap_or_else(|| Value::String(String::new())));
            out.insert("files".into(), m.remove("files").unwrap_or_else(|| Value::Object(Map::new())));
            out.insert("edits".into(), m.remove("edits").unwrap_or_else(|| Value::Array(Vec::new())));
            out.insert("disputes".into(), m.remove("disputes").unwrap_or_else(|| Value::Object(Map::new())));
            out.insert("script".into(), Value::Null);
            return Ok(Value::Object(out));
        }
    }
    let mut lines = py::splitlines(stripped, true);
    if !lines.is_empty() && lines[0].starts_with("```") && py::strip(lines[lines.len() - 1]) == "```" {
        // a fence around the whole reply
        lines = if lines.len() > 2 { lines[1..lines.len() - 1].to_vec() } else { Vec::new() };
    }
    let mut blocks: Vec<(String, String, Vec<&str>)> = Vec::new();
    for line in lines {
        let bare = line.trim_end_matches(['\r', '\n']);
        if bare == "@@@" {
            // a bare marker: WITH right after a REPLACE body, else END
            let kind = if blocks.last().is_some_and(|b| b.0 == "REPLACE") { "WITH" } else { "END" };
            blocks.push((kind.into(), String::new(), Vec::new()));
        } else if line.starts_with(MARK) || ["@@@ WITH", "@@@ END", "@@@ RATIONALE"].contains(&bare) {
            let rest = py::strip(&line[4..]);
            let (kind, arg) = rest.split_once(' ').unwrap_or((rest, ""));
            blocks.push((upper(kind), py::strip(arg).to_string(), Vec::new()));
        } else if let Some(last) = blocks.last_mut() {
            last.2.push(line); // text before the first block is ignored
        }
    }
    if blocks.is_empty() {
        return Err(Error::value("no @@@ blocks in the reply"));
    }
    let (mut rationale, mut files, mut edits, mut disputes, mut script) =
        (String::new(), Map::new(), Vec::new(), Map::new(), None::<String>);
    let mut i = 0;
    while i < blocks.len() {
        let (kind, arg, body) = &blocks[i];
        let content = body.concat();
        match kind.as_str() {
            "RATIONALE" => rationale = py::strip(&content).to_string(),
            "FILE" => {
                if arg.is_empty() || files.contains_key(arg) {
                    return Err(Error::value(format!("FILE block {}: needs one path, given once", i + 1)));
                }
                files.insert(arg.clone(), Value::String(format!("{}\n", content.trim_end_matches('\n'))));
            }
            "REPLACE" => {
                if arg.is_empty()
                    || i + 2 >= blocks.len()
                    || blocks[i + 1].0 != "WITH"
                    || blocks[i + 2].0 != "END"
                    || !py::strip(&blocks[i + 2].2.concat()).is_empty()
                {
                    return Err(Error::value(format!(
                        "REPLACE block {} ({arg}): must be followed by @@@ WITH and @@@ END",
                        i + 1
                    )));
                }
                // Trailing newlines are trimmed on both sides, so a blank separator line cannot break the anchor.
                let mut edit = Map::new();
                edit.insert("path".into(), Value::String(arg.clone()));
                edit.insert("old".into(), Value::String(content.trim_end_matches('\n').into()));
                edit.insert("new".into(), Value::String(blocks[i + 1].2.concat().trim_end_matches('\n').into()));
                edits.push(Value::Object(edit));
                i += 2;
            }
            "END" => {} // a terminator after a FILE or DISPUTE block is harmless; text after it is not a file
            "SCRIPT" => {
                if script.is_some() {
                    return Err(Error::value(format!("SCRIPT block {}: give at most one script", i + 1)));
                }
                script = Some(format!("{}\n", content.trim_matches('\n')));
            }
            "DISPUTE" => {
                if arg.is_empty() {
                    return Err(Error::value(format!("DISPUTE block {}: needs a test id", i + 1)));
                }
                disputes.insert(arg.clone(), Value::String(py::strip(&content).into()));
            }
            _ => return Err(Error::value(format!("block {}: unknown @@@ {kind}", i + 1))),
        }
        i += 1;
    }
    let mut reply = Map::new();
    reply.insert("rationale".into(), Value::String(rationale));
    reply.insert("files".into(), Value::Object(files));
    reply.insert("edits".into(), Value::Array(edits));
    reply.insert("disputes".into(), Value::Object(disputes));
    reply.insert("script".into(), script.map_or(Value::Null, Value::String));
    Ok(Value::Object(reply))
}

/// `files.update(src)` with Python's rules for a dict, a list of pairs or a str; Ok(false) when it put a
/// non-str key or value in, which file_map then refuses.
fn update(files: &mut Files, src: &Value) -> Result<bool> {
    let mut clean = true;
    let mut put = |files: &mut Files, key: Item, value: Item| -> Result<()> {
        let key = match key {
            Item::S(s) => s,
            Item::V(Value::String(s)) => s.clone(),
            Item::V(v @ (Value::Array(_) | Value::Object(_))) => return Err(unhashable(v)),
            Item::V(_) => {
                clean = false;
                return Ok(());
            }
        };
        match value.text() {
            Some(text) => files.insert(key, Value::String(text.into())),
            None => {
                clean = false;
                files.insert(key, Value::Null)
            }
        };
        Ok(())
    };
    match src {
        Value::Object(m) => {
            for (k, v) in m {
                put(files, Item::S(k.clone()), Item::V(v))?;
            }
        }
        Value::Array(elements) => {
            for (i, element) in elements.iter().enumerate() {
                let pair: Vec<Item> = match element {
                    Value::Array(_) | Value::Object(_) | Value::String(_) => py_iter(element)?,
                    _ => return Err(type_error("object is not iterable")),
                };
                if pair.len() != 2 {
                    return Err(Error::value(format!(
                        "dictionary update sequence element #{i} has length {}; 2 is required",
                        pair.len()
                    )));
                }
                let mut pair = pair.into_iter();
                let (k, v) = (pair.next().expect("two"), pair.next().expect("two"));
                put(files, k, v)?;
            }
        }
        Value::String(s) => {
            if !s.is_empty() {
                return Err(Error::value("dictionary update sequence element #0 has length 1; 2 is required"));
            }
        }
        _ => return Err(type_error(format!("'{}' object is not iterable", type_name(src)))),
    }
    Ok(clean)
}

/// Complete files from a parsed text reply: FILE blocks replace whole files, REPLACE blocks are exact edits
/// matched against the text the writer was shown (materialize's rules), files not named stay unchanged.
pub fn materialize_text(current: &Files, declared: &Files, reply: &Value) -> Result<Files> {
    let whole = getitem(reply, "files")?;
    for path in py_iter(whole)? {
        let known = match &path {
            Item::S(s) => declared.contains_key(s),
            Item::V(v) => in_dict(declared, v)?,
        };
        if !known {
            return Err(Error::value(format!("FILE {} is not a declared editable file", path.repr())));
        }
    }
    let edits = getitem(reply, "edits")?;
    for edit in py_iter(edits)? {
        if py_in(edit.get("path")?, whole)? {
            return Err(Error::value("a file cannot be both replaced whole and edited in one reply"));
        }
    }
    let mut files = current.clone();
    if truthy(edits) {
        // materialize(current, declared, {'edits': reply['edits'], 'rationale': reply['rationale']})
        if !getitem(reply, "rationale")?.is_string() {
            return Err(Error::value("reply needs rationale and either files or edits, not both"));
        }
        files = apply_edits(current, declared, edits)?;
    }
    if !update(&mut files, whole)? {
        return Err(Error::value("candidate files must map relative paths to complete UTF-8 text"));
    }
    check_files(&files, "candidate files")?;
    Ok(files)
}

// ---------------------------------------------------------------------------------------------------------------
// Spec tests: Python's ast decides what compiles, so the Python logic runs in the interpreter Ringo would run on

const SPEC_TEST_CODE: &str = r#"import ast, json, re, sys


def spec_test_file(text):
    """The Python test module in a spec-test writer's reply (its largest fenced block, or the whole reply),
    checked to compile; with the ids of its test methods."""
    import ast
    import re
    blocks = re.findall(r'```(?:python|py)?[^\n]*\n(.*?)```', text, flags=re.S)
    source = max(blocks, key=len) if blocks else text
    try:
        tree = ast.parse(source)
    except SyntaxError:
        # A brief sentence quoted as a four-quote docstring does not compile; read its quotes as three.
        source = re.sub(r'"{4}(.*?)"{4}', r'"""\1"""', source)
        tree = ast.parse(source)
    ids = [f'{node.name}.{item.name}' for node in tree.body if isinstance(node, ast.ClassDef)
           for item in node.body if isinstance(item, ast.FunctionDef) and item.name.startswith('test')]
    if not ids:
        raise ValueError('the spec-test reply defines no test methods')
    return source, ids


def test_sources(source):
    """{'Class.test': source text} for each test method, so a failing test can be shown with its own code."""
    import ast
    tree = ast.parse(source)
    return {f'{node.name}.{item.name}': ast.get_source_segment(source, item) or ''
            for node in tree.body if isinstance(node, ast.ClassDef)
            for item in node.body if isinstance(item, ast.FunctionDef) and item.name.startswith('test')}


text = sys.stdin.buffer.read().decode('utf-8')
try:
    if sys.argv[1] == 'spec_test_file':
        out = {'ok': list(spec_test_file(text))}
    else:
        out = {'ok': list(test_sources(text).items())}
except BaseException as exc:
    out = {'err': [type(exc).__name__, str(exc)]}
sys.stdout.write(json.dumps(out))
"#;

#[cfg(unix)]
fn run_spec_code(function: &str, text: &str) -> Result<Value> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let python = crate::python();
    let mut child = Command::new(&python)
        .args(["-I", "-B", "-c", SPEC_TEST_CODE, function])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::os(py::os_error(&e, Some(&python.to_string_lossy()))))?;
    child.stdin.take().expect("piped stdin").write_all(text.as_bytes())?;
    let output = child.wait_with_output()?;
    let reply = pyjson::loads(&String::from_utf8_lossy(&output.stdout)).map_err(|_| {
        Error::os(format!("{function} failed in Python: {}", String::from_utf8_lossy(&output.stderr).trim_end()))
    })?;
    if let Some(ok) = reply.get("ok") {
        return Ok(ok.clone());
    }
    let err = reply.get("err").and_then(Value::as_array);
    let (name, msg) = match err.map(|e| (e[0].as_str(), e[1].as_str())) {
        Some((Some(name), Some(msg))) => (name, msg),
        _ => return Err(Error::os(format!("{function}: unreadable reply from Python"))),
    };
    Err(match name {
        "ValueError" => Error::value(msg),
        // run() catches SyntaxError beside ValueError; anything else would propagate out of run() in Python.
        "SyntaxError" | "IndentationError" | "TabError" => typed(name, msg),
        _ => Error::new(Exc::Type, msg).with_data(serde_json::json!({ "python_type": name })),
    })
}

/// The Python test module in a spec-test writer's reply (its largest fenced block, or the whole reply), checked
/// to compile; with the ids of its test methods. Runs workflow.py's own code in `crate::python()`.
pub fn spec_test_file(text: &str) -> Result<(String, Vec<String>)> {
    #[cfg(not(unix))]
    {
        let _ = text;
        return Err(Error::os(PLATFORM));
    }
    #[cfg(unix)]
    {
        let ok = run_spec_code("spec_test_file", text)?;
        let source = ok[0].as_str().unwrap_or_default().to_string();
        let ids = ok[1].as_array().map_or_else(Vec::new, |a| a.iter().filter_map(Value::as_str).map(String::from).collect());
        Ok((source, ids))
    }
}

/// {'Class.test': source text} for each test method, so a failing test can be shown with its own code.
pub fn test_sources(source: &str) -> Result<Map<String, Value>> {
    #[cfg(not(unix))]
    {
        let _ = source;
        return Err(Error::os(PLATFORM));
    }
    #[cfg(unix)]
    {
        let ok = run_spec_code("test_sources", source)?;
        let mut out = Map::new();
        for pair in ok.as_array().into_iter().flatten() {
            if let (Some(id), Some(text)) = (pair[0].as_str(), pair[1].as_str()) {
                out.insert(id.into(), Value::String(text.into()));
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Numbers: NUMBER, matched by hand with re's Unicode \w and \d, and decimal.Decimal(...).normalize()

/// workflow.NUMBER's pattern; `numbers` implements it directly (its lookbehinds need Python's \w).
pub const NUMBER: &str = r"(?:(?<![\w.])[-\u2212])?(?<![A-Za-z_])\d[\d,]*(?:\.\d+)?";

const PREC: i64 = 28;
const EMAX: i64 = 999_999;
const EMIN: i64 = -999_999;
const ETINY: i64 = EMIN - PREC + 1;
const ETOP: i64 = EMAX - PREC + 1;

/// A normalized `decimal.Decimal` (default context: 28 digits, ROUND_HALF_EVEN, Emax 999999). Equal values are
/// equal, -0 and 0 included, as in a Python set.
#[derive(Clone, Debug)]
pub struct Num {
    neg: bool,
    digits: Vec<u8>,
    exp: i64,
}

pub type Numbers = BTreeSet<Num>;

fn overflow() -> Error {
    typed("Overflow", "[<class 'decimal.Overflow'>]")
}

impl Num {
    fn is_zero(&self) -> bool {
        self.digits == *b"0"
    }

    fn sign(&self) -> i8 {
        if self.is_zero() {
            0
        } else if self.neg {
            -1
        } else {
            1
        }
    }

    /// `float(self)`.
    pub fn to_f64(&self) -> f64 {
        self.to_string().parse().unwrap_or(f64::NAN)
    }

    /// Decimal(token).normalize() for a token of an optional '-', decimal digits and an optional '.' part.
    fn normalized(token: &str) -> Result<Num> {
        let neg = token.starts_with('-');
        let body = token.trim_start_matches('-');
        let (int, frac) = body.split_once('.').unwrap_or((body, ""));
        let digit = |c: char| b'0' + decimal_digit(c).unwrap_or(0) as u8;
        let mut digits: Vec<u8> = int.chars().chain(frac.chars()).map(digit).collect();
        let lead = digits.iter().take_while(|&&d| d == b'0').count();
        digits.drain(..lead.min(digits.len().saturating_sub(1)));
        if digits.is_empty() {
            digits.push(b'0');
        }
        let (digits, exp) = Self::fix(digits, -(frac.chars().count() as i64))?;
        if digits == *b"0" {
            return Ok(Num { neg, digits, exp: 0 });
        }
        let (mut digits, mut exp) = (digits, exp);
        while digits.len() > 1 && digits[digits.len() - 1] == b'0' && exp < EMAX {
            digits.pop();
            exp += 1;
        }
        Ok(Num { neg, digits, exp })
    }

    /// Decimal._fix in the default context.
    fn fix(digits: Vec<u8>, exp: i64) -> Result<(Vec<u8>, i64)> {
        if digits == *b"0" {
            return Ok((digits, exp.clamp(ETINY, EMAX)));
        }
        let len = digits.len() as i64;
        let mut exp_min = len + exp - PREC;
        if exp_min > ETOP {
            return Err(overflow());
        }
        exp_min = exp_min.max(ETINY);
        if exp >= exp_min {
            return Ok((digits, exp));
        }
        let mut keep = len + exp - exp_min;
        let mut work = digits;
        if keep < 0 {
            work = vec![b'1'];
            keep = 0;
        }
        let keep = keep as usize;
        let rest = &work[keep..];
        let exact_half = rest[0] == b'5' && rest[1..].iter().all(|&d| d == b'0');
        let up = if exact_half && (keep == 0 || (work[keep - 1] - b'0').is_multiple_of(2)) {
            false
        } else {
            rest[0] >= b'5'
        };
        let mut coeff: Vec<u8> = if keep == 0 { vec![b'0'] } else { work[..keep].to_vec() };
        if up {
            let mut i = coeff.len();
            loop {
                if i == 0 {
                    coeff.insert(0, b'1');
                    break;
                }
                i -= 1;
                if coeff[i] == b'9' {
                    coeff[i] = b'0';
                } else {
                    coeff[i] += 1;
                    break;
                }
            }
            if coeff.len() as i64 > PREC {
                coeff.pop();
                exp_min += 1;
            }
        }
        if exp_min > ETOP {
            return Err(overflow());
        }
        Ok((coeff, exp_min))
    }
}

impl std::fmt::Display for Num {
    /// `str(Decimal)`: scientific notation when the exponent is positive or the value is below 1e-6.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let digits = std::str::from_utf8(&self.digits).expect("ascii digits");
        let n = digits.len() as i64;
        let leftdigits = self.exp + n;
        let dotplace = if self.exp <= 0 && leftdigits > -6 { leftdigits } else { 1 };
        let (intpart, fracpart) = if dotplace <= 0 {
            ("0".to_string(), format!(".{}{digits}", "0".repeat((-dotplace) as usize)))
        } else if dotplace >= n {
            (format!("{digits}{}", "0".repeat((dotplace - n) as usize)), String::new())
        } else {
            (digits[..dotplace as usize].to_string(), format!(".{}", &digits[dotplace as usize..]))
        };
        let exp = if leftdigits == dotplace { String::new() } else { format!("E{:+}", leftdigits - dotplace) };
        write!(f, "{}{intpart}{fracpart}{exp}", if self.neg { "-" } else { "" })
    }
}

impl Ord for Num {
    fn cmp(&self, other: &Num) -> Ordering {
        let (a, b) = (self.sign(), other.sign());
        if a != b || a == 0 {
            return a.cmp(&b);
        }
        let adjusted = |x: &Num| x.exp + x.digits.len() as i64;
        let magnitude = adjusted(self).cmp(&adjusted(other)).then_with(|| self.digits.cmp(&other.digits));
        if a < 0 { magnitude.reverse() } else { magnitude }
    }
}

impl PartialOrd for Num {
    fn partial_cmp(&self, other: &Num) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Num {
    fn eq(&self, other: &Num) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Num {}

/// The end of a NUMBER match whose first digit is at `i`.
fn digits_end(c: &[char], mut i: usize) -> usize {
    i += 1;
    while i < c.len() && (c[i] == ',' || decimal_digit(c[i]).is_some()) {
        i += 1;
    }
    if i + 1 < c.len() && c[i] == '.' && decimal_digit(c[i + 1]).is_some() {
        i += 2;
        while i < c.len() && decimal_digit(c[i]).is_some() {
            i += 1;
        }
    }
    i
}

/// The signed numeric values in text, normalized so 2.00 == 2, 09 == 9 and 1,628.62 == 1628.62; a minus sign
/// (ASCII or U+2212) counts unless it follows a digit or letter, as in a date or a range. Digits glued to a
/// letter, such as a source ID like S4, are names, not values. Err only where Decimal raises Overflow (an
/// integer part of more than a million digits), which Python's run() does not catch.
pub fn numbers(text: &str) -> Result<Numbers> {
    let c: Vec<char> = text.chars().collect();
    let mut found = Numbers::new();
    let mut p = 0;
    while p < c.len() {
        let minus = (c[p] == '-' || c[p] == '\u{2212}')
            && (p == 0 || !(is_word(c[p - 1]) || c[p - 1] == '.'))
            && p + 1 < c.len()
            && decimal_digit(c[p + 1]).is_some();
        let plain = !minus
            && decimal_digit(c[p]).is_some()
            && (p == 0 || !(c[p - 1].is_ascii_alphabetic() || c[p - 1] == '_'));
        if !minus && !plain {
            p += 1;
            continue;
        }
        let end = digits_end(&c, if minus { p + 1 } else { p });
        let token: String = c[p..end].iter().filter(|&&ch| ch != ',').map(|&ch| if ch == '\u{2212}' { '-' } else { ch }).collect();
        found.insert(Num::normalized(&token)?);
        p = end;
    }
    Ok(found)
}

// ---------------------------------------------------------------------------------------------------------------
// harness.py functions load_spec needs. Phase 1 stand-ins, ported here so load_spec can be checked now; phase 2
// switches to crate::rt::harness's ports of the same functions.

const JUDGE_PRESETS: [(&str, &str); 3] = [
    (
        "claude",
        "claude -p --setting-sources project --strict-mcp-config --disable-slash-commands --tools \"\" \
         --no-session-persistence --model sonnet",
    ),
    (
        "codex",
        "codex exec --ignore-user-config --ignore-rules --disable apps --disable plugins \
         -c 'web_search=\"disabled\"' -c 'model_reasoning_effort=\"high\"' --skip-git-repo-check --ephemeral \
         -s read-only --color never --json -",
    ),
    (
        "codex-chatgpt",
        "codex exec --ignore-user-config --ignore-rules --disable apps --disable plugins \
         --disable hooks --disable shell_tool --disable unified_exec --disable skill_search \
         --enable skip_host_skill_discovery --disable unbounded_connection_retries \
         --disable sleep_tool --disable goals --disable view_image --disable image_generation \
         --disable default_mode_request_user_input -c include_environment_context=false \
         -c include_permissions_instructions=false -c skills.include_instructions=false \
         -c include_collaboration_mode_instructions=false \
         -c 'project_doc_max_bytes=0' -c 'forced_login_method=\"chatgpt\"' \
         -c 'model_provider=\"openai\"' -c 'web_search=\"disabled\"' \
         --skip-git-repo-check --ephemeral -s read-only --color never --json -",
    ),
];

/// shlex.split(command): POSIX words, quotes and escapes, nothing expanded.
fn shlex_split(command: &str) -> Result<Vec<String>> {
    let ws = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    let mut chars = command.chars();
    let mut out = Vec::new();
    let mut state = Some(' ');
    loop {
        let (mut token, mut quoted, mut escaped_state) = (String::new(), false, ' ');
        loop {
            let next = chars.next();
            match state {
                None => break,
                Some(' ') => match next {
                    None => {
                        state = None;
                        break;
                    }
                    Some(c) if ws(c) => {
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some('\\') => {
                        escaped_state = 'a';
                        state = Some('\\');
                    }
                    Some(q @ ('\'' | '"')) => state = Some(q),
                    Some(c) => {
                        token.push(c);
                        state = Some('a');
                    }
                },
                Some(q @ ('\'' | '"')) => {
                    quoted = true;
                    match next {
                        None => return Err(Error::value("No closing quotation")),
                        Some(c) if c == q => state = Some('a'),
                        Some('\\') if q == '"' => {
                            escaped_state = q;
                            state = Some('\\');
                        }
                        Some(c) => token.push(c),
                    }
                }
                Some('\\') => {
                    let Some(c) = next else { return Err(Error::value("No escaped character")) };
                    // In posix shells, only the quote itself or the escape character may be escaped within quotes.
                    if (escaped_state == '\'' || escaped_state == '"') && c != '\\' && c != escaped_state {
                        token.push('\\');
                    }
                    token.push(c);
                    state = Some(escaped_state);
                }
                Some(_) => match next {
                    None => {
                        state = None;
                        break;
                    }
                    Some(c) if ws(c) => {
                        state = Some(' ');
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some(q @ ('\'' | '"')) => state = Some(q),
                    Some('\\') => {
                        escaped_state = 'a';
                        state = Some('\\');
                    }
                    Some(c) => token.push(c),
                },
            }
        }
        if token.is_empty() && !quoted {
            return Ok(out);
        }
        out.push(token);
    }
}

fn strings(argv: &[String]) -> Value {
    Value::Array(argv.iter().map(|a| Value::String(a.clone())).collect())
}

/// harness.backend: {'name', 'argv', 'style', ...} for a backend spec, or ValueError.
fn backend(spec: &str) -> Result<Value> {
    let preset = |name: &str| JUDGE_PRESETS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c);
    let mut extra: Vec<(&str, Value)> = Vec::new();
    let (argv, style) = if spec == "claude" {
        (shlex_split(preset("claude").expect("preset"))?, "claude")
    } else if let Some(command) = spec.strip_prefix("claude:") {
        (shlex_split(command)?, "claude")
    } else if spec == "codex" || spec == "codex-chatgpt" {
        let chatgpt = spec == "codex-chatgpt";
        extra.push(("subscription_auth", Value::Bool(chatgpt)));
        extra.push(("lean", Value::Bool(chatgpt)));
        extra.push(("tools", strings(&if chatgpt { vec![] } else { vec!["command_execution".into()] })));
        (shlex_split(preset(spec).expect("preset"))?, "codex")
    } else if let Some(command) = spec.strip_prefix("codex:") {
        let argv = shlex_split(command)?;
        if !argv.is_empty() && !argv.iter().any(|a| a == "--json") {
            return Err(Error::value(format!(
                "backend {}: a codex: command must pass --json so its events can be parsed",
                repr(spec)
            )));
        }
        extra.push(("subscription_auth", Value::Bool(false)));
        extra.push((
            "tools",
            strings(&if argv.iter().any(|a| a == "--search") { vec!["web_search".into()] } else { vec![] }),
        ));
        (argv, "codex")
    } else if let Some(model) = spec.strip_prefix("ollama:") {
        (if model.is_empty() { vec![] } else { vec!["ollama".into(), "run".into(), model.into()] }, "plain")
    } else {
        (shlex_split(spec)?, "plain")
    };
    if argv.is_empty() {
        return Err(Error::value(format!("backend {} has no command", repr(spec))));
    }
    let mut out = Map::new();
    out.insert("name".into(), Value::String(spec.into()));
    out.insert("argv".into(), strings(&argv));
    out.insert("style".into(), Value::String(style.into()));
    for (k, v) in extra {
        out.insert(k.into(), v);
    }
    Ok(Value::Object(out))
}

/// harness.tool_access.
fn tool_access(name: &str) -> &'static str {
    if name == "claude" || name == "codex-chatgpt" || name.starts_with("ollama:") {
        "none"
    } else if name == "codex" {
        "disk"
    } else {
        "unknown"
    }
}

/// harness.check_backends.
fn check_backends(names: &[&str], allow_tools: bool) -> Result<()> {
    let risky: Vec<&&str> = names.iter().filter(|n| tool_access(n) != "none").collect();
    if !risky.is_empty() && !allow_tools {
        let list = risky.iter().map(|n| repr(n)).collect::<Vec<_>>().join(", ");
        return Err(Error::value(format!(
            "backends may read local files: [{list}]; use external isolation or explicitly set allow_tool_backends"
        )));
    }
    Ok(())
}

/// harness.with_model: argv with its --model value replaced by model (or --model model appended).
fn with_model(argv: &[String], model: &str) -> Vec<String> {
    let (mut out, mut replaced, mut k) = (Vec::new(), false, 0);
    while k < argv.len() {
        let arg = &argv[k];
        if arg == "--model" || arg.starts_with("--model=") {
            if !replaced {
                out.extend(["--model".to_string(), model.to_string()]);
                replaced = true;
            }
            k += if arg == "--model" && k + 1 < argv.len() { 2 } else { 1 };
        } else {
            out.push(arg.clone());
            k += 1;
        }
    }
    if !replaced {
        out.extend(["--model".to_string(), model.to_string()]);
    }
    out
}

/// harness.valid_price(x) is not None.
fn valid_price(v: &Value) -> bool {
    match py_num(v) {
        Some(PyNum::Float(f)) => f.is_finite() && f >= 0.0,
        Some(PyNum::Int(t)) => !matches!(v, Value::Bool(_)) && !t.starts_with('-'),
        None => false,
    }
}

/// backend with a role's model and reasoning effort applied. Codex takes both as CLI options before its trailing
/// '-'; a claude-style backend takes a model only. (Ported now because load_spec validates roles with it; the
/// backend is the dict harness.backend returns.)
pub fn role_backend(backend: &Value, opts: Option<&Value>) -> Result<Value> {
    let Some(opts) = opts.filter(|o| truthy(o)) else { return Ok(backend.clone()) };
    let argv: Vec<String> = py_iter(getitem(backend, "argv")?)?
        .iter()
        .map(|a| a.text().unwrap_or_default().to_string())
        .collect();
    let model = get(opts, "model")?.filter(|m| truthy(m));
    let effort = get(opts, "effort")?.filter(|e| truthy(e));
    let style = getitem(backend, "style")?;
    let argv = if *style == "codex" {
        let mut extra = Vec::new();
        if let Some(m) = model {
            extra.extend(["--model".to_string(), py_str(m)]);
        }
        if let Some(e) = effort {
            extra.extend(["-c".to_string(), format!("model_reasoning_effort=\"{}\"", py_str(e))]);
        }
        if argv.last().is_some_and(|a| a == "-") {
            let mut out = argv[..argv.len() - 1].to_vec();
            out.extend(extra);
            out.push("-".into());
            out
        } else {
            [argv, extra].concat()
        }
    } else if *style == "claude" {
        if effort.is_some() {
            return Err(Error::value("a claude-style backend takes a role model, not an effort"));
        }
        match model {
            Some(m) => with_model(&argv, &py_str(m)),
            None => argv,
        }
    } else {
        return Err(Error::value(format!(
            "role options need a codex or claude backend, not {}",
            py_str(getitem(backend, "name")?)
        )));
    };
    let mut out = backend.as_object().cloned().unwrap_or_default();
    out.insert("argv".into(), strings(&argv));
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------------------------------------------
// load_spec

pub fn positive<'a>(value: &'a Value, name: &str, integer: bool) -> Result<&'a Value> {
    let ok = match (value, py_num(value)) {
        (Value::Number(_), Some(PyNum::Int(t))) => t != "0" && !t.starts_with('-'),
        (Value::Number(_), Some(PyNum::Float(f))) => !integer && f.is_finite() && f > 0.0,
        _ => false,
    };
    if !ok || !valid_price(value) {
        return Err(Error::value(format!("{name} must be positive{}", if integer { " integer" } else { "" })));
    }
    Ok(value)
}

/// A JSON int's value, when it is an int (not a float or bool) that fits in i128.
fn int_of(v: &Value) -> Option<i128> {
    match v {
        Value::Number(n) if !is_float_text(&n.to_string()) => n.to_string().parse().ok(),
        _ => None,
    }
}

fn is_int(v: &Value) -> bool {
    matches!(v, Value::Number(n) if !is_float_text(&n.to_string()))
}

/// Path.read_text(encoding='utf-8'): strict UTF-8 (UnicodeDecodeError's text) and universal newlines.
fn read_text(bytes: &[u8]) -> Result<String> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            let start = e.valid_up_to();
            let (end, reason) = match e.error_len() {
                None => (bytes.len(), "unexpected end of data"),
                Some(n) if (0xc2..=0xf4).contains(&bytes[start]) => (start + n, "invalid continuation byte"),
                Some(n) => (start + n, "invalid start byte"),
            };
            let msg = if end - start == 1 {
                format!("'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}", bytes[start])
            } else {
                format!("'utf-8' codec can't decode bytes in position {start}-{}: {reason}", end - 1)
            };
            return Err(typed("UnicodeDecodeError", msg));
        }
    };
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// sys.executable of the interpreter that would run Ringo (`crate::python()`), for "{python}" in a spec.
#[cfg(unix)]
fn sys_executable() -> Result<String> {
    static EXE: std::sync::OnceLock<std::result::Result<String, String>> = std::sync::OnceLock::new();
    EXE.get_or_init(|| {
        let python = crate::python();
        let out = std::process::Command::new(&python)
            .args(["-I", "-c", "import sys; sys.stdout.write(sys.executable)"])
            .output()
            .map_err(|e| py::os_error(&e, Some(&python.to_string_lossy())))?;
        if !out.status.success() {
            return Err(format!("{} cannot report sys.executable", python.to_string_lossy()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    })
    .clone()
    .map_err(Error::os)
}

fn need(cond: bool, msg: &str) -> Result<()> {
    if cond { Err(Error::value(msg)) } else { Ok(()) }
}

fn nonempty_str(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).is_some_and(|s| !s.is_empty())
}

pub fn load_spec(path: &Path) -> Result<Value> {
    #[cfg(not(unix))]
    {
        let _ = path;
        return Err(Error::os(PLATFORM));
    }
    #[cfg(unix)]
    {
        let name = posix_str(&path.to_string_lossy());
        let bytes = std::fs::read(path).map_err(|e| os_error(&e, Some(&name)))?;
        let mut spec = Guard(strict_json(&read_text(&bytes)?)?);
        check_spec(&mut spec.0, &name)?;
        Ok(std::mem::take(&mut spec.0))
    }
}

#[cfg(unix)]
fn check_spec(spec: &mut Value, path: &str) -> Result<()> {
    const ALLOWED: [&str; 26] = [
        "version", "brief", "material", "files", "fixtures", "checks", "council", "rubric", "writer", "judge",
        "samples", "first_judges", "max_rounds", "max_seconds", "timeout_s", "max_cost", "unpriced_call_usd",
        "allow_tool_backends", "allow_execution", "roles", "protocol", "spec_tests", "candidates", "scripts",
        "protocol_repair", "no_regression",
    ];
    if spec.as_object().is_some_and(|m| m.contains_key("max_calls")) {
        return Err(Error::value("max_calls was removed: calls are set by max_rounds and samples"));
    }
    let shape = spec.as_object().filter(|m| {
        m.keys().all(|k| ALLOWED.contains(&k.as_str()))
            && m.get("version").is_some_and(|v| py_eq(v, &Value::from(1)))
    });
    let Some(m) = shape else {
        return Err(Error::value("a workflow needs version 1 and only documented fields"));
    };
    let council = m.contains_key("council");
    need(
        !council && ["rubric", "samples", "first_judges"].iter().any(|k| m.contains_key(*k)),
        "rubric, samples and first_judges configure the Council: name a council bridge or remove them",
    )?;
    let required: &[&str] = if council { &["brief", "writer", "judge", "rubric"] } else { &["brief", "writer", "judge"] };
    for key in required {
        if !m.get(*key).and_then(Value::as_str).is_some_and(|s| !py::strip(s).is_empty()) {
            return Err(Error::value(format!("{key} is required")));
        }
    }
    need(!m.get("material").is_none_or(Value::is_string), "material must be text")?;
    file_map(m.get("files").unwrap_or(&Value::Null), "files")?;
    let m = spec.as_object_mut().expect("checked above");
    file_map(m.entry("fixtures").or_insert_with(|| Value::Object(Map::new())), "fixtures")?;
    let (files, fixtures) = (m["files"].as_object().expect("files"), m["fixtures"].as_object().expect("fixtures"));
    need(files.is_empty() || files.keys().any(|k| fixtures.contains_key(k)), "editable files must be nonempty and disjoint from fixtures")?;
    let mut names: Vec<&String> = files.keys().chain(fixtures.keys()).collect();
    names.sort();
    let collides = names.iter().any(|name| {
        let (_, tail) = posix_parse(name);
        (0..tail.len()).rev().any(|k| {
            let parent = tail[..k].join("/");
            let parent = if parent.is_empty() { ".".to_string() } else { parent };
            names.iter().any(|n| **n == parent)
        })
    });
    need(collides, "a file cannot also be a directory")?;
    for (key, default) in [("max_rounds", 3), ("max_seconds", 600), ("timeout_s", 120)] {
        positive(m.entry(key).or_insert(Value::from(default)), key, key == "max_rounds")?;
    }
    if council {
        let samples = positive(m.entry("samples").or_insert(Value::from(6)), "samples", true)?;
        let samples = int_of(samples).filter(|s| (2..=25).contains(s)).ok_or_else(|| Error::value("samples must be from 2 to 25"))?;
        // The Council decides first on this many judges and calls the rest only if they do not recommend.
        let first = m.entry("first_judges").or_insert(Value::from(samples.min(3.max((samples + 1) / 2)) as i64));
        need(
            !is_int(first) || !int_of(first).is_some_and(|f| 2 <= f && f <= samples),
            "first_judges must be an integer from 2 to samples",
        )?;
    }
    for key in ["allow_execution", "allow_tool_backends"] {
        if !m.entry(key).or_insert(Value::Bool(false)).is_boolean() {
            return Err(Error::value(format!("{key} must be boolean")));
        }
    }
    let (writer, judge) = (m["writer"].as_str().expect("writer").to_string(), m["judge"].as_str().expect("judge").to_string());
    check_backends(&[&writer, &judge], m["allow_tool_backends"] == Value::Bool(true))?;
    backend(&writer)?; // Reject invalid command syntax before inference.
    backend(&judge)?;
    let protocol = m.entry("protocol").or_insert(Value::String("json".into()));
    need(!(*protocol == "text" || *protocol == "json"), "protocol must be text or json")?;
    let roles = m.entry("roles").or_insert_with(|| Value::Object(Map::new()));
    let Some(roles) = roles.as_object().filter(|r| r.keys().all(|k| ROLES.contains(&k.as_str()))) else {
        return Err(Error::value(format!("roles may only name {}", ROLES.join(", "))));
    };
    for (role, opts) in roles {
        let valid = opts.as_object().is_some_and(|o| {
            o.keys().all(|k| k == "model" || k == "effort")
                && o.values().all(|v| nonempty_str(Some(v)))
                && o.get("effort").and_then(Value::as_str).is_none_or(|e| EFFORTS.contains(&e))
        });
        if !valid {
            return Err(Error::value(format!("roles.{role} takes model and effort ({}) only", EFFORTS.join("/"))));
        }
        role_backend(&backend(if role == "review" { &judge } else { &writer })?, Some(opts))?;
    }
    if let Some(tests) = m.get_mut("spec_tests").filter(|t| !t.is_null()) {
        need(council, "spec_tests drive the review loop; they do not combine with the Council")?;
        let Some(t) = tests.as_object_mut().filter(|t| t.keys().all(|k| k == "python" || k == "timeout_s") && nonempty_str(t.get("python"))) else {
            return Err(Error::value("spec_tests needs the python executable that runs the generated unittest module"));
        };
        positive(t.entry("timeout_s").or_insert(Value::from(120)), "spec_tests timeout_s", false)?;
    }
    for key in ["protocol_repair", "no_regression"] {
        if !m.entry(key).or_insert(Value::Bool(false)).is_boolean() {
            return Err(Error::value(format!("{key} must be boolean")));
        }
    }
    let candidates = positive(m.entry("candidates").or_insert(Value::from(1)), "candidates", true)?;
    let candidates = int_of(candidates).filter(|c| (1..=4).contains(c)).ok_or_else(|| Error::value("candidates must be from 1 to 4"))?;
    need(candidates > 1 && council, "candidates select by checks and spec tests; they do not combine with the Council")?;
    let text_protocol = m["protocol"] == "text";
    if let Some(scripts) = m.get_mut("scripts").filter(|s| !s.is_null()) {
        let Some(s) = scripts.as_object_mut().filter(|s| s.keys().all(|k| k == "python" || k == "timeout_s") && nonempty_str(s.get("python"))) else {
            return Err(Error::value("scripts needs the python executable that runs a writer script"));
        };
        need(!text_protocol, "scripts need the text protocol")?;
        positive(s.entry("timeout_s").or_insert(Value::from(60)), "scripts timeout_s", false)?;
    }
    need(
        m.get("max_cost").is_some_and(|v| !v.is_null()) && m.get("unpriced_call_usd").is_none_or(Value::is_null),
        "max_cost needs unpriced_call_usd for attempts that report no cost",
    )?;
    let command = m.get("council");
    need(
        council
            && !command.and_then(Value::as_array).is_some_and(|c| !c.is_empty() && c.iter().all(|x| nonempty_str(Some(x)))),
        "council must be an explicit argv for the offline bridge",
    )?;
    let checks = m.entry("checks").or_insert_with(|| Value::Array(Vec::new()));
    need(!checks.as_array().is_some_and(|c| !c.is_empty()), "at least one independent executable check is required")?;
    need(m["allow_execution"] != Value::Bool(true), "checks execute code: explicitly set allow_execution")?;
    for check in m.get_mut("checks").and_then(Value::as_array_mut).expect("checks") {
        let valid = check.as_object().is_some_and(|c| {
            c.keys().all(|k| k == "name" || k == "argv" || k == "timeout_s")
                && nonempty_str(c.get("name"))
                && c.get("argv").and_then(Value::as_array).is_some_and(|a| !a.is_empty() && a.iter().all(|x| nonempty_str(Some(x))))
        });
        need(!valid, "each check needs name and nonempty argv")?;
        let c = check.as_object_mut().expect("checked");
        positive(c.entry("timeout_s").or_insert(Value::from(30)), "check timeout_s", false)?;
    }
    // "{python}" in a command is the interpreter running Ringo, so one spec runs on macOS, Linux and Windows alike.
    for check in m.get_mut("checks").and_then(Value::as_array_mut).expect("checks") {
        for arg in check["argv"].as_array_mut().expect("argv") {
            if *arg == "{python}" {
                *arg = Value::String(sys_executable()?);
            }
        }
    }
    for key in ["spec_tests", "scripts"] {
        if let Some(section) = m.get_mut(key).filter(|s| truthy(s))
            && section.get("python").is_some_and(|p| *p == "{python}")
        {
            section["python"] = Value::String(sys_executable()?);
        }
    }
    // The caller selects executable bridge/check commands. They are trusted code, not an OS boundary around
    // model-generated artifacts.
    if council {
        let resolved = realpath(&posix_str(path))?;
        let parent = match resolved.rfind('/') {
            Some(0) | None => "/".to_string(),
            Some(i) => resolved[..i].to_string(),
        };
        let rubric = m["rubric"].as_str().expect("rubric");
        let joined = if rubric.starts_with('/') { posix_str(rubric) } else { posix_str(&format!("{parent}/{rubric}")) };
        m.insert("rubric".into(), Value::String(realpath(&joined)?));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Writing

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The first non-finite float in serialization order, as json.dumps(allow_nan=False) refuses it.
fn non_finite(value: &Value) -> Option<String> {
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            Value::Array(items) => stack.extend(items.iter().rev()),
            Value::Object(map) => stack.extend(map.values().rev()),
            Value::Number(n) => {
                let t = n.to_string();
                if is_float_text(&t) || t.contains(['N', 'I']) {
                    let f = match t.as_str() {
                        "NaN" => f64::NAN,
                        "Infinity" => f64::INFINITY,
                        "-Infinity" => f64::NEG_INFINITY,
                        _ => t.parse().unwrap_or(0.0),
                    };
                    if !f.is_finite() {
                        return Some(if f.is_nan() { "nan" } else if f > 0.0 { "inf" } else { "-inf" }.into());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(bad) = non_finite(value) {
        return Err(Error::value(format!("Out of range float values are not JSON compliant: {bad}")));
    }
    let text = pyjson::dumps_with(value, pyjson::Opts { indent: Some(2), ensure_ascii: false, ..Default::default() });
    std::fs::write(path, text + "\n").map_err(|e| os_error(&e, Some(&path_text(path))))
}

pub fn digest(files: &Files) -> String {
    use sha2::{Digest, Sha256};
    let text = pyjson::dumps_with(
        &Value::Object(files.clone()),
        pyjson::Opts { sort_keys: true, ensure_ascii: false, ..Default::default() },
    );
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// pathlib's mkdir(parents=True, exist_ok=True).
fn mkdir_parents(path: &Path) -> Result<()> {
    let path = if path.as_os_str().is_empty() { Path::new(".") } else { path };
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match path.parent() {
                Some(parent) if parent != path => mkdir_parents(parent)?,
                _ => return Err(os_error(&e, Some(&path_text(path)))),
            }
            match std::fs::create_dir(path) {
                Err(e) if !path.is_dir() => Err(os_error(&e, Some(&path_text(path)))),
                _ => Ok(()),
            }
        }
        Err(e) if path.is_dir() => {
            let _ = e;
            Ok(())
        }
        Err(e) => Err(os_error(&e, Some(&path_text(path)))),
    }
}

pub fn write_files(root: &Path, files: &Files) -> Result<()> {
    for (name, text) in files {
        let path = root.join(name);
        mkdir_parents(path.parent().unwrap_or(root))?;
        let Some(text) = text.as_str() else {
            return Err(type_error(format!("data must be str, not {}", type_name(text))));
        };
        std::fs::write(&path, text).map_err(|e| os_error(&e, Some(&path_text(&path))))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// The review's material, with difflib

/// The parts of Python's difflib that review_material uses: SequenceMatcher (isjunk None, with and without the
/// autojunk popularity heuristic), its matching blocks and opcodes, and unified_diff with its defaults.
pub mod difflib {
    use std::collections::HashMap;

    /// (i, j, size): a[i..i+size] == b[j..j+size].
    pub type Match = (usize, usize, usize);
    pub type Opcode = (&'static str, usize, usize, usize, usize);

    pub struct SequenceMatcher {
        a: Vec<usize>,
        b: Vec<usize>,
        b2j: HashMap<usize, Vec<usize>>,
    }

    impl SequenceMatcher {
        /// SequenceMatcher(None, a, b, autojunk) over lines (or any strings), compared by equality.
        pub fn new<'s>(a: &[&'s str], b: &[&'s str], autojunk: bool) -> SequenceMatcher {
            let mut ids: HashMap<&'s str, usize> = HashMap::new();
            let mut intern = |s: &'s str| {
                let n = ids.len();
                *ids.entry(s).or_insert(n)
            };
            let a: Vec<usize> = a.iter().map(|s| intern(s)).collect();
            let b: Vec<usize> = b.iter().map(|s| intern(s)).collect();
            let mut b2j: HashMap<usize, Vec<usize>> = HashMap::new();
            for (i, &elt) in b.iter().enumerate() {
                b2j.entry(elt).or_default().push(i);
            }
            // Purge popular elements: those that make up more than 1% of a sequence of 200 or more.
            if autojunk && b.len() >= 200 {
                let ntest = b.len() / 100 + 1;
                b2j.retain(|_, idxs| idxs.len() <= ntest);
            }
            SequenceMatcher { a, b, b2j }
        }

        pub fn find_longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> Match {
            let (a, b) = (&self.a, &self.b);
            let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0);
            let mut j2len: HashMap<usize, usize> = HashMap::new();
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            for (i, elt) in a.iter().enumerate().take(ahi).skip(alo) {
                newj2len.clear();
                if let Some(js) = self.b2j.get(elt) {
                    for &j in js {
                        if j < blo {
                            continue;
                        }
                        if j >= bhi {
                            break;
                        }
                        let k = if j > 0 { j2len.get(&(j - 1)).copied().unwrap_or(0) } else { 0 } + 1;
                        newj2len.insert(j, k);
                        if k > bestsize {
                            (besti, bestj, bestsize) = (i + 1 - k, j + 1 - k, k);
                        }
                    }
                }
                std::mem::swap(&mut j2len, &mut newj2len);
            }
            // Extend the best by equal elements on each end ("popular" ones are not in b2j; there is no junk).
            while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
                (besti, bestj, bestsize) = (besti - 1, bestj - 1, bestsize + 1);
            }
            while besti + bestsize < ahi && bestj + bestsize < bhi && a[besti + bestsize] == b[bestj + bestsize] {
                bestsize += 1;
            }
            (besti, bestj, bestsize)
        }

        pub fn get_matching_blocks(&self) -> Vec<Match> {
            let (la, lb) = (self.a.len(), self.b.len());
            let mut queue = vec![(0, la, 0, lb)];
            let mut blocks = Vec::new();
            while let Some((alo, ahi, blo, bhi)) = queue.pop() {
                let (i, j, k) = self.find_longest_match(alo, ahi, blo, bhi);
                if k > 0 {
                    blocks.push((i, j, k));
                    if alo < i && blo < j {
                        queue.push((alo, i, blo, j));
                    }
                    if i + k < ahi && j + k < bhi {
                        queue.push((i + k, ahi, j + k, bhi));
                    }
                }
            }
            blocks.sort();
            let (mut i1, mut j1, mut k1) = (0, 0, 0);
            let mut out = Vec::new();
            for (i2, j2, k2) in blocks {
                if i1 + k1 == i2 && j1 + k1 == j2 {
                    k1 += k2;
                } else {
                    if k1 > 0 {
                        out.push((i1, j1, k1));
                    }
                    (i1, j1, k1) = (i2, j2, k2);
                }
            }
            if k1 > 0 {
                out.push((i1, j1, k1));
            }
            out.push((la, lb, 0));
            out
        }

        pub fn get_opcodes(&self) -> Vec<Opcode> {
            let (mut i, mut j) = (0, 0);
            let mut out = Vec::new();
            for (ai, bj, size) in self.get_matching_blocks() {
                let tag = if i < ai && j < bj {
                    "replace"
                } else if i < ai {
                    "delete"
                } else if j < bj {
                    "insert"
                } else {
                    ""
                };
                if !tag.is_empty() {
                    out.push((tag, i, ai, j, bj));
                }
                (i, j) = (ai + size, bj + size);
                if size > 0 {
                    out.push(("equal", ai, i, bj, j));
                }
            }
            out
        }

        pub fn get_grouped_opcodes(&self, n: usize) -> Vec<Vec<Opcode>> {
            let mut codes = self.get_opcodes();
            if codes.is_empty() {
                codes.push(("equal", 0, 1, 0, 1));
            }
            if codes[0].0 == "equal" {
                let (tag, i1, i2, j1, j2) = codes[0];
                codes[0] = (tag, i1.max(i2.saturating_sub(n)), i2, j1.max(j2.saturating_sub(n)), j2);
            }
            let last = codes.len() - 1;
            if codes[last].0 == "equal" {
                let (tag, i1, i2, j1, j2) = codes[last];
                codes[last] = (tag, i1, i2.min(i1 + n), j1, j2.min(j1 + n));
            }
            let nn = n + n;
            let mut groups = Vec::new();
            let mut group = Vec::new();
            for (tag, mut i1, i2, mut j1, j2) in codes {
                // End the current group and start a new one whenever there is a large range with no changes.
                if tag == "equal" && i2 - i1 > nn {
                    group.push((tag, i1, i2.min(i1 + n), j1, j2.min(j1 + n)));
                    groups.push(std::mem::take(&mut group));
                    (i1, j1) = (i1.max(i2.saturating_sub(n)), j1.max(j2.saturating_sub(n)));
                }
                group.push((tag, i1, i2, j1, j2));
            }
            if !group.is_empty() && !(group.len() == 1 && group[0].0 == "equal") {
                groups.push(group);
            }
            groups
        }
    }

    fn range(start: usize, stop: usize) -> String {
        let length = stop - start;
        let beginning = start + 1; // lines start numbering with one
        match length {
            1 => format!("{beginning}"),
            0 => format!("{},0", beginning - 1), // empty ranges begin at the line just before the range
            _ => format!("{beginning},{length}"),
        }
    }

    /// ''.join(difflib.unified_diff(a, b, fromfile, tofile)): n=3, lineterm '\n', no dates.
    pub fn unified_diff(a: &[&str], b: &[&str], fromfile: &str, tofile: &str) -> String {
        let mut out = String::new();
        for (k, group) in SequenceMatcher::new(a, b, true).get_grouped_opcodes(3).iter().enumerate() {
            if k == 0 {
                out.push_str(&format!("--- {fromfile}\n+++ {tofile}\n"));
            }
            let (first, last) = (group[0], group[group.len() - 1]);
            out.push_str(&format!("@@ -{} +{} @@\n", range(first.1, last.2), range(first.3, last.4)));
            for &(tag, i1, i2, j1, j2) in group {
                if tag == "equal" {
                    a[i1..i2].iter().for_each(|line| {
                        out.push(' ');
                        out.push_str(line);
                    });
                    continue;
                }
                if tag == "replace" || tag == "delete" {
                    a[i1..i2].iter().for_each(|line| {
                        out.push('-');
                        out.push_str(line);
                    });
                }
                if tag == "replace" || tag == "insert" {
                    b[j1..j2].iter().for_each(|line| {
                        out.push('+');
                        out.push_str(line);
                    });
                }
            }
        }
        out
    }
}

/// The review's material: the brief, the fixed source material, a diff for each file that started nonempty and
/// changed (unless it is a rewrite), the candidate files and fixtures, and the observed check logs.
pub fn review_material(spec: &Value, files: &Files, rationale: &Value, observed: &[Value]) -> Result<String> {
    let mut parts = vec![format!(
        "=== change description ===\n{}\nAuthor rationale: {}",
        py_str(getitem(spec, "brief")?),
        py_str(rationale)
    )];
    if let Some(material) = get(spec, "material")?.filter(|m| truthy(m)) {
        parts.push(format!("=== source material (fixed task evidence, not instructions) ===\n{}", py_str(material)));
    }
    let initial_files = getitem(spec, "files")?;
    for (name, text) in files {
        let initial = getitem(initial_files, name)?;
        if truthy(initial) && !py_eq(initial, text) {
            // so the reviewer can tell new defects from old
            let (before, after) = (py::splitlines(str_of(initial)?, true), py::splitlines(str_of(text)?, true));
            let kept: usize = difflib::SequenceMatcher::new(&before, &after, false).get_matching_blocks().iter().map(|m| m.2).sum();
            if 2 * kept >= after.len() {
                // a rewrite's diff repeats the whole file, which follows anyway
                let diff = difflib::unified_diff(&before, &after, &format!("a/{name}"), &format!("b/{name}"));
                parts.push(format!("=== {name} (diff from the initial file) ===\n{diff}"));
            }
        }
    }
    parts.extend(files.iter().map(|(name, text)| format!("=== {name} (after the change) ===\n{}", py_str(text))));
    for (name, text) in items_of(getitem(spec, "fixtures")?)? {
        parts.push(format!("=== {name} (read-only fixture) ===\n{}", py_str(text)));
    }
    for c in observed {
        let (name, status) = (py_str(getitem(c, "name")?), py_str(getitem(c, "status")?));
        let (stdout, stderr) = (py_str(getitem(c, "stdout")?), py_str(getitem(c, "stderr")?));
        parts.push(format!("=== observed check: {name} ({status}) ===\n{stdout}{stderr}"));
    }
    Ok(parts.join("\n\n"))
}

// ---------------------------------------------------------------------------------------------------------------
// Unicode tables (ranges of hex code points), generated with Python's str.isalnum() / '_' (WORD), str.isdecimal()
// (DECIMAL), not str.isprintable() (UNPRINTABLE) and str.upper() != str (UPPER).
// Generated from Python 3.14.7, Unicode 16.0.0.
const WORD: &str = "\
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
const DECIMAL: &str = "\
30-39 660-669 6F0-6F9 7C0-7C9 966-96F 9E6-9EF A66-A6F AE6-AEF B66-B6F BE6-BEF C66-C6F CE6-CEF D66-D6F DE6-DEF \
E50-E59 ED0-ED9 F20-F29 1040-1049 1090-1099 17E0-17E9 1810-1819 1946-194F 19D0-19D9 1A80-1A89 1A90-1A99 \
1B50-1B59 1BB0-1BB9 1C40-1C49 1C50-1C59 A620-A629 A8D0-A8D9 A900-A909 A9D0-A9D9 A9F0-A9F9 AA50-AA59 ABF0-ABF9 \
FF10-FF19 104A0-104A9 10D30-10D39 10D40-10D49 11066-1106F 110F0-110F9 11136-1113F 111D0-111D9 112F0-112F9 \
11450-11459 114D0-114D9 11650-11659 116C0-116C9 116D0-116E3 11730-11739 118E0-118E9 11950-11959 11BF0-11BF9 \
11C50-11C59 11D50-11D59 11DA0-11DA9 11F50-11F59 16130-16139 16A60-16A69 16AC0-16AC9 16B50-16B59 16D70-16D79 \
1CCF0-1CCF9 1D7CE-1D7FF 1E140-1E149 1E2F0-1E2F9 1E4F0-1E4F9 1E5F1-1E5FA 1E950-1E959 1FBF0-1FBF9";
const UNPRINTABLE: &str = "\
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
const UPPER: &str = "\
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
