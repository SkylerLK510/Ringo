//! JSON text exactly as Python's `json.dumps` writes it, for every record and message the Python modules build
//! with it: insertion-ordered objects, `ensure_ascii` escapes, Python's separators and indentation, and Python's
//! float repr (shortest round trip; exponent form below 1e-4 and from 1e16). Values are `serde_json::Value`
//! built with the `preserve_order` and `arbitrary_precision` features, so key order and big integers survive.

use crate::rt::{py, Error, Exc, Result};
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
    // Python's repr: the fewest digits that round-trip, correctly rounded (ties to even). Rust's shortest form has
    // the right number of digits but may round an exact tie up, so the digits are re-made with exact formatting.
    let shortest = format!("{f:e}");
    let count = shortest.split_once('e').unwrap().0.chars().filter(char::is_ascii_digit).count();
    let sci = format!("{f:.*e}", count - 1);
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

/// A float as the JSON value json.dumps writes for it: an infinity as a number that overflows to it (dumped as
/// Infinity); NaN, which serde_json cannot hold, as null.
pub fn float_value(f: f64) -> Value {
    if let Some(n) = Number::from_f64(f) {
        return Value::Number(n);
    }
    if f.is_nan() {
        return Value::Null;
    }
    Value::Number((if f > 0.0 { "1e400" } else { "-1e400" }).parse().expect("a JSON number"))
}

/// `json.loads(text)`, with Python's error texts.
pub fn loads(text: &str) -> Result<Value> {
    Decoder::default().loads(text).map_err(|f| f.into_error(text))
}

/// An empty insertion-ordered object.
pub fn object() -> Map<String, Value> {
    Map::new()
}

// ---------------------------------------------------------------------------------------------------------------
// json.loads, as CPython 3.14's C scanner runs it

/// Nesting levels a decoded value may have by default, where Rust keeps it: serde_json values are dropped and
/// written recursively, and a few thousand levels overflow a 2 MB thread stack. Deeper input is a RecursionError
/// here, where Python 3.14 still parses it (a reported deviation); 1000 is Python's own default recursion limit.
pub const MAX_DEPTH: usize = 1_000;
/// `sys.get_int_max_str_digits()` default: a longer integer makes `int()` (and so json) raise ValueError.
pub const INT_MAX_STR_DIGITS: usize = 4300;

/// How a JSON read failed.
#[derive(Debug)]
pub enum JsonFail {
    /// `json.JSONDecodeError` (a ValueError): `msg` at code point `pos`; `into_error` formats its text.
    Decode { msg: &'static str, pos: usize },
    /// `RecursionError` while entering a nested JSON `what` ("object" or "array").
    Recursion { what: &'static str },
    /// An exception a hook or `int()` raised, propagated unchanged.
    Raised(Error),
}

impl JsonFail {
    /// The exception as Python reports it for the document `doc`.
    pub fn into_error(self, doc: &str) -> Error {
        match self {
            JsonFail::Decode { msg, pos } => Error::new(Exc::JsonDecode, decode_error_text(msg, doc.chars(), pos)),
            JsonFail::Recursion { what } => recursion_error(what),
            JsonFail::Raised(e) => e,
        }
    }
}

fn recursion_error(what: &str) -> Error {
    Error::new(Exc::Recursion, format!("maximum recursion depth exceeded while decoding a JSON {what} from a unicode string"))
}

/// `str(JSONDecodeError(msg, doc, pos))`: "msg: line L column C (char pos)".
pub fn decode_error_text(msg: &str, doc: impl Iterator<Item = char>, pos: usize) -> String {
    let (mut line, mut last) = (1, None);
    for (i, c) in doc.take(pos).enumerate() {
        if c == '\n' {
            line += 1;
            last = Some(i);
        }
    }
    let col = last.map_or(pos + 1, |i| pos - i);
    format!("{msg}: line {line} column {col} (char {pos})")
}

/// A hook taking a number or constant's text (`parse_float`, `parse_constant`).
pub type TextHook<'a> = &'a dyn Fn(&str) -> Result<Value>;
/// `object_pairs_hook`: an object's pairs in order; called innermost first, the outermost object last.
pub type PairsHook<'a> = &'a dyn Fn(Vec<(String, Value)>) -> Result<Value>;

/// `json.JSONDecoder(parse_constant=..., parse_float=..., object_pairs_hook=...)` as CPython 3.14's C scanner runs
/// it (strict strings, integers through `int()`). Positions are code points. Nesting past `max_depth` is a
/// RecursionError. `Decoder::default()` is `json.loads`'s own.
pub struct Decoder<'a> {
    pub parse_constant: TextHook<'a>,
    pub parse_float: TextHook<'a>,
    pub object_pairs_hook: PairsHook<'a>,
    pub max_depth: usize,
    /// `max_depth` is CPython's own stack-bound limit, under which its Python hooks need room: the pairs hook and
    /// parse_constant raise RecursionError 3 levels short of it, parse_float 4.
    pub hook_headroom: bool,
    /// Python keeps an unpaired surrogate escape in a str, which a Rust string cannot hold: by default it becomes
    /// U+FFFD; refused, an otherwise valid parse fails with the UnicodeEncodeError that the first UTF-8 encode of
    /// the first such string raises.
    pub refuse_surrogates: bool,
}

impl Default for Decoder<'static> {
    /// `json.loads(text)`'s hooks: NaN and the infinities parse (NaN, which serde_json cannot hold, as null),
    /// floats through `float()`, objects as plain dicts.
    fn default() -> Self {
        Decoder {
            parse_constant: &python_constant,
            parse_float: &python_float,
            object_pairs_hook: &dict,
            max_depth: MAX_DEPTH,
            hook_headroom: false,
            refuse_surrogates: false,
        }
    }
}

fn python_constant(name: &str) -> Result<Value> {
    Ok(float_value(match name {
        "NaN" => f64::NAN,
        "-Infinity" => f64::NEG_INFINITY,
        _ => f64::INFINITY,
    }))
}

fn python_float(text: &str) -> Result<Value> {
    py::float(text).map(float_value)
}

/// The default object hook: a plain dict (a repeated key keeps its first position and its last value).
pub fn dict(pairs: Vec<(String, Value)>) -> Result<Value> {
    let mut map = Map::new();
    for (k, v) in pairs {
        map.insert(k, v);
    }
    Ok(Value::Object(map))
}

/// The first unpaired surrogate of a parse: its string's position, the number of surrogates in a row there, the
/// first one.
type Lone = (usize, usize, u32);

/// The UnicodeEncodeError of encoding a string with a lone surrogate to UTF-8.
fn surrogate_error((at, run, unit): Lone) -> Error {
    Error::new(
        Exc::UnicodeEncode,
        if run == 1 {
            format!("'utf-8' codec can't encode character '\\u{unit:04x}' in position {at}: surrogates not allowed")
        } else {
            format!("'utf-8' codec can't encode characters in position {at}-{}: surrogates not allowed", at + run - 1)
        },
    )
}

enum Frame {
    Array(Vec<Value>),
    Object(Vec<(String, Value)>, String),
}

/// The open containers of a parse; dropped without recursion, however deep their finished parts are.
struct Frames(Vec<Frame>);

impl Drop for Frames {
    fn drop(&mut self) {
        for frame in self.0.drain(..) {
            match frame {
                Frame::Array(items) => items.into_iter().for_each(dispose),
                Frame::Object(pairs, _) => pairs.into_iter().for_each(|(_, v)| dispose(v)),
            }
        }
    }
}

/// Drops a value of any depth without recursion.
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

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

pub(crate) fn skip_ws(s: &[char], mut i: usize) -> usize {
    while i < s.len() && is_ws(s[i]) {
        i += 1;
    }
    i
}

fn fail(msg: &'static str, pos: usize) -> JsonFail {
    JsonFail::Decode { msg, pos }
}

fn starts(s: &[char], at: usize, word: &str) -> bool {
    word.chars().enumerate().all(|(k, c)| s.get(at + k) == Some(&c))
}

impl Decoder<'_> {
    /// `json.loads(text, ...)` for a str: a leading BOM is refused, whitespace around the value is allowed.
    pub fn loads(&self, text: &str) -> std::result::Result<Value, JsonFail> {
        let s: Vec<char> = text.chars().collect();
        if s.first() == Some(&'\u{feff}') {
            return Err(fail("Unexpected UTF-8 BOM (decode using utf-8-sig)", 0));
        }
        self.decode(&s)
    }

    /// `json.loads(data, ...)` for bytes: the encoding detected as json does (UTF-8, -16 or -32), decoded with
    /// surrogatepass (an unpaired surrogate as U+FFFD). A decode failure is a ValueError, as Python's
    /// JSONDecodeError and UnicodeDecodeError are; the UnicodeDecodeError text is not Python's.
    pub fn loads_bytes(&self, data: &[u8]) -> std::result::Result<Value, JsonFail> {
        let s = decode_bytes(data).map_err(JsonFail::Raised)?;
        self.decode(&s).map_err(|e| match e {
            JsonFail::Decode { msg, pos } => JsonFail::Raised(Error::new(Exc::JsonDecode, decode_error_text(msg, s.iter().copied(), pos))),
            other => other,
        })
    }

    /// `JSONDecoder.decode(s)`: one value, whitespace around it, "Extra data" after it.
    pub fn decode(&self, s: &[char]) -> std::result::Result<Value, JsonFail> {
        let mut lone = None;
        let (value, end) = self.scan(s, skip_ws(s, 0), &mut lone)?;
        let end = skip_ws(s, end);
        if end != s.len() {
            dispose(value);
            return Err(fail("Extra data", end));
        }
        match lone {
            Some(found) if self.refuse_surrogates => {
                dispose(value);
                Err(JsonFail::Raised(surrogate_error(found)))
            }
            _ => Ok(value),
        }
    }

    /// `JSONDecoder.raw_decode(s, idx)`: the value starting at code point idx and the index after it.
    pub fn raw_decode(&self, s: &[char], idx: usize) -> std::result::Result<(Value, usize), JsonFail> {
        self.scan(s, idx, &mut None)
    }

    fn scan(&self, s: &[char], mut idx: usize, lone: &mut Option<Lone>) -> std::result::Result<(Value, usize), JsonFail> {
        let n = s.len();
        // the hooks' Python frames need this much room under max_depth
        let (hook_room, float_room) = if self.hook_headroom { (3, 4) } else { (0, 0) };
        let mut frames = Frames(Vec::new());
        let stack = &mut frames.0;
        'scan: loop {
            // one value at idx, or the start of a container
            let (mut value, mut end) = 'value: {
                if idx >= n {
                    return Err(fail("Expecting value", idx)); // StopIteration(idx)
                }
                match s[idx] {
                    '"' => {
                        let (text, end) = scanstring(s, idx + 1, lone)?;
                        (Value::String(text), end)
                    }
                    '{' => {
                        if stack.len() >= self.max_depth {
                            return Err(JsonFail::Recursion { what: "object" });
                        }
                        let i = skip_ws(s, idx + 1);
                        if i < n && s[i] == '}' {
                            if stack.len() + 1 + hook_room > self.max_depth {
                                return Err(JsonFail::Recursion { what: "object" });
                            }
                            break 'value ((self.object_pairs_hook)(Vec::new()).map_err(JsonFail::Raised)?, i + 1);
                        }
                        let (key, i) = key(s, i, lone)?;
                        stack.push(Frame::Object(Vec::new(), key));
                        idx = i;
                        continue 'scan;
                    }
                    '[' => {
                        if stack.len() >= self.max_depth {
                            return Err(JsonFail::Recursion { what: "array" });
                        }
                        let i = skip_ws(s, idx + 1);
                        if i < n && s[i] == ']' {
                            break 'value (Value::Array(Vec::new()), i + 1);
                        }
                        stack.push(Frame::Array(Vec::new()));
                        idx = i;
                        continue 'scan;
                    }
                    'n' if idx + 3 < n && starts(s, idx + 1, "ull") => (Value::Null, idx + 4),
                    't' if idx + 3 < n && starts(s, idx + 1, "rue") => (Value::Bool(true), idx + 4),
                    'f' if idx + 4 < n && starts(s, idx + 1, "alse") => (Value::Bool(false), idx + 5),
                    'N' if idx + 2 < n && starts(s, idx + 1, "aN") => (self.constant(stack.len() + hook_room, "NaN")?, idx + 3),
                    'I' if idx + 7 < n && starts(s, idx + 1, "nfinity") => {
                        (self.constant(stack.len() + hook_room, "Infinity")?, idx + 8)
                    }
                    '-' if idx + 8 < n && starts(s, idx + 1, "Infinity") => {
                        (self.constant(stack.len() + hook_room, "-Infinity")?, idx + 9)
                    }
                    _ => self.number(s, idx, stack.len() + float_room)?,
                }
            };
            // attach the value to the open containers, closing every one that ends here
            loop {
                let Some(top) = stack.last_mut() else {
                    return Ok((value, end));
                };
                let close = if matches!(top, Frame::Array(_)) { ']' } else { '}' };
                match top {
                    Frame::Array(items) => items.push(value),
                    Frame::Object(pairs, key) => pairs.push((std::mem::take(key), value)),
                }
                let i = skip_ws(s, end);
                if i < n && s[i] == close {
                    if close == '}' && stack.len() + hook_room > self.max_depth {
                        return Err(JsonFail::Recursion { what: "object" });
                    }
                    value = match stack.pop() {
                        Some(Frame::Array(items)) => Value::Array(items),
                        Some(Frame::Object(pairs, _)) => (self.object_pairs_hook)(pairs).map_err(JsonFail::Raised)?,
                        None => unreachable!("a container is open"),
                    };
                    end = i + 1;
                    continue;
                }
                if i >= n || s[i] != ',' {
                    return Err(fail("Expecting ',' delimiter", i));
                }
                let j = skip_ws(s, i + 1);
                if j < n && s[j] == close {
                    return Err(fail(
                        if close == ']' { "Illegal trailing comma before end of array" } else { "Illegal trailing comma before end of object" },
                        i,
                    ));
                }
                idx = j;
                if let Some(Frame::Object(_, pending)) = stack.last_mut() {
                    let (k, after) = key(s, j, lone)?;
                    *pending = k;
                    idx = after;
                }
                break;
            }
        }
    }

    /// parse_constant, with `depth` the levels it runs at.
    fn constant(&self, depth: usize, name: &str) -> std::result::Result<Value, JsonFail> {
        if depth > self.max_depth {
            return Err(JsonFail::Recursion { what: "object" });
        }
        (self.parse_constant)(name).map_err(JsonFail::Raised)
    }

    /// `_match_number_unicode`: an integer (through `int()`, with its digit limit) or a float (parse_float, run at
    /// `depth` levels).
    fn number(&self, s: &[char], start: usize, depth: usize) -> std::result::Result<(Value, usize), JsonFail> {
        let end_idx = s.len() - 1;
        let digit = |i: usize| s[i].is_ascii_digit();
        let mut idx = start;
        if s[idx] == '-' {
            idx += 1;
            if idx > end_idx {
                return Err(fail("Expecting value", start));
            }
        }
        if ('1'..='9').contains(&s[idx]) {
            idx += 1;
            while idx <= end_idx && digit(idx) {
                idx += 1;
            }
        } else if s[idx] == '0' {
            idx += 1;
        } else {
            return Err(fail("Expecting value", start));
        }
        let mut is_float = false;
        if idx < end_idx && s[idx] == '.' && digit(idx + 1) {
            is_float = true;
            idx += 2;
            while idx <= end_idx && digit(idx) {
                idx += 1;
            }
        }
        if idx < end_idx && (s[idx] == 'e' || s[idx] == 'E') {
            let e_start = idx;
            idx += 1;
            if idx < end_idx && (s[idx] == '-' || s[idx] == '+') {
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
        let text: String = s[start..idx].iter().collect();
        if is_float {
            if depth > self.max_depth {
                return Err(JsonFail::Recursion { what: "object" });
            }
            return Ok(((self.parse_float)(&text).map_err(JsonFail::Raised)?, idx));
        }
        let digits = text.trim_start_matches('-').len();
        if digits > INT_MAX_STR_DIGITS {
            return Err(JsonFail::Raised(Error::value(format!(
                "Exceeds the limit ({INT_MAX_STR_DIGITS} digits) for integer string conversion: value has {digits} \
                 digits; use sys.set_int_max_str_digits() to increase the limit"
            ))));
        }
        let n: Number = text.parse().expect("an integer token is a JSON number");
        Ok((Value::Number(n), idx))
    }
}

/// An object key, its ':' and the whitespace before the value (`_parse_object_unicode`).
fn key(s: &[char], i: usize, lone: &mut Option<Lone>) -> std::result::Result<(String, usize), JsonFail> {
    if i >= s.len() || s[i] != '"' {
        return Err(fail("Expecting property name enclosed in double quotes", i));
    }
    let (k, i) = scanstring(s, i + 1, lone)?;
    let i = skip_ws(s, i);
    if i >= s.len() || s[i] != ':' {
        return Err(fail("Expecting ':' delimiter", i));
    }
    Ok((k, skip_ws(s, i + 1)))
}

fn hex4(s: &[char], at: usize) -> Option<u32> {
    s[at..at + 4].iter().try_fold(0u32, |acc, c| c.to_digit(16).map(|d| (acc << 4) | d))
}

/// `scanstring_unicode(s, end, strict=True)`: the string whose opening quote is at end - 1, and the index after it.
/// An unpaired surrogate becomes U+FFFD; the parse's first is noted in `lone`.
fn scanstring(s: &[char], mut end: usize, lone: &mut Option<Lone>) -> std::result::Result<(String, usize), JsonFail> {
    let (n, begin) = (s.len(), end - 1);
    let mut out = String::new();
    // code points so far, and whether they end in this string's first run of surrogates
    let (mut count, mut first_run, mut in_run) = (0, lone.is_none(), false);
    loop {
        let mut next = end;
        while next < n && s[next] != '"' && s[next] != '\\' {
            if (s[next] as u32) <= 0x1f {
                return Err(fail("Invalid control character at", next));
            }
            next += 1;
        }
        if next >= n {
            return Err(fail("Unterminated string starting at", begin));
        }
        out.extend(&s[end..next]);
        if next > end {
            count += next - end;
            in_run = false;
        }
        let terminator = s[next];
        next += 1;
        if terminator == '"' {
            return Ok((out, next));
        }
        if next == n {
            return Err(fail("Unterminated string starting at", begin));
        }
        let esc = s[next];
        if esc != 'u' {
            end = next + 1;
            out.push(match esc {
                '"' => '"',
                '\\' => '\\',
                '/' => '/',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                _ => return Err(fail("Invalid \\escape", end - 2)),
            });
            count += 1;
            in_run = false;
            continue;
        }
        next += 1;
        end = next + 4;
        if end > n {
            return Err(fail("Invalid \\uXXXX escape", next - 1));
        }
        let mut c = hex4(s, next).ok_or_else(|| fail("Invalid \\uXXXX escape", end - 5))?;
        if (0xd800..=0xdbff).contains(&c) && end + 6 <= n && s[end] == '\\' && s[end + 1] == 'u' {
            let c2 = hex4(s, end + 2).ok_or_else(|| fail("Invalid \\uXXXX escape", end + 1))?;
            if (0xdc00..=0xdfff).contains(&c2) {
                c = 0x10000 + ((c - 0xd800) << 10) + (c2 - 0xdc00);
                end += 6;
            }
        }
        out.push(char::from_u32(c).unwrap_or('\u{fffd}')); // an unpaired surrogate: Python keeps it
        if (0xd800..=0xdfff).contains(&c) && first_run {
            match lone {
                Some((_, run, _)) if in_run => *run += 1,
                Some(_) => first_run = false,
                None => *lone = Some((count, 1, c)),
            }
            in_run = first_run;
        } else {
            in_run = false;
        }
        count += 1;
    }
}

/// `json.detect_encoding(b)` then `b.decode(encoding, 'surrogatepass')`, unpaired surrogates as U+FFFD.
fn decode_bytes(b: &[u8]) -> Result<Vec<char>> {
    let bad = |enc: &str| Error::value(format!("'{enc}' codec can't decode the bytes"));
    let units = |b: &[u8], size: usize, big: bool| -> Vec<u32> {
        b.chunks(size)
            .map(|c| c.iter().enumerate().fold(0u32, |acc, (i, &x)| acc | ((x as u32) << (8 * if big { size - 1 - i } else { i }))))
            .collect()
    };
    let utf16 = |b: &[u8], big: bool, enc: &str| -> Result<Vec<char>> {
        if !b.len().is_multiple_of(2) {
            return Err(bad(enc));
        }
        let u: Vec<u16> = units(b, 2, big).into_iter().map(|x| x as u16).collect();
        Ok(char::decode_utf16(u).map(|r| r.unwrap_or('\u{fffd}')).collect())
    };
    let utf32 = |b: &[u8], big: bool, enc: &str| -> Result<Vec<char>> {
        if !b.len().is_multiple_of(4) {
            return Err(bad(enc));
        }
        units(b, 4, big)
            .into_iter()
            .map(|x| match x {
                0xd800..=0xdfff => Ok('\u{fffd}'),
                _ => char::from_u32(x).ok_or_else(|| bad(enc)),
            })
            .collect()
    };
    match b {
        [0, 0, 0xfe, 0xff, rest @ ..] => utf32(rest, true, "utf-32"),
        [0xff, 0xfe, 0, 0, rest @ ..] => utf32(rest, false, "utf-32"),
        [0xfe, 0xff, rest @ ..] => utf16(rest, true, "utf-16"),
        [0xff, 0xfe, rest @ ..] => utf16(rest, false, "utf-16"),
        [0xef, 0xbb, 0xbf, rest @ ..] => utf8(rest).ok_or_else(|| bad("utf-8-sig")),
        [0, x, _, _, ..] => {
            if *x != 0 {
                utf16(b, true, "utf-16-be")
            } else {
                utf32(b, true, "utf-32-be")
            }
        }
        [_, 0, x, y, ..] => {
            if *x != 0 || *y != 0 {
                utf16(b, false, "utf-16-le")
            } else {
                utf32(b, false, "utf-32-le")
            }
        }
        [0, _] => utf16(b, true, "utf-16-be"),
        [_, 0] => utf16(b, false, "utf-16-le"),
        _ => utf8(b).ok_or_else(|| bad("utf-8")),
    }
}

/// Strict UTF-8 that also passes encoded surrogates (ED A0..BF xx), as U+FFFD.
fn utf8(mut b: &[u8]) -> Option<Vec<char>> {
    let mut out = Vec::with_capacity(b.len());
    loop {
        match std::str::from_utf8(b) {
            Ok(t) => {
                out.extend(t.chars());
                return Some(out);
            }
            Err(e) => {
                let (ok, rest) = b.split_at(e.valid_up_to());
                out.extend(std::str::from_utf8(ok).expect("valid prefix").chars());
                match rest {
                    [0xed, 0xa0..=0xbf, 0x80..=0xbf, ..] => {
                        out.push('\u{fffd}');
                        b = &rest[3..];
                    }
                    _ => return None,
                }
            }
        }
    }
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
            (6264016581616065.0 / 4.0, "1566004145404016.2"), // an exact tie: ...016.25
            (0.5, "0.5"),
            (2.5e-5, "2.5e-05"),
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

    #[test]
    fn infinity_survives_as_python_writes_it() {
        assert_eq!(dumps(&float_value(f64::INFINITY)), "Infinity");
        assert_eq!(dumps(&float_value(f64::NEG_INFINITY)), "-Infinity");
        assert_eq!(dumps(&float_value(1.5)), "1.5");
        assert_eq!(float_value(f64::NAN), Value::Null);
    }

    #[test]
    fn json_errors_read_as_python_writes_them() {
        let err = |text: &str| loads(text).map_err(|e| (e.type_name(), e.msg)).err();
        assert_eq!(err("[1,]"), Some(("JSONDecodeError", "Illegal trailing comma before end of array: line 1 column 3 (char 2)".into())));
        assert_eq!(err("{\"a\" 1}"), Some(("JSONDecodeError", "Expecting ':' delimiter: line 1 column 6 (char 5)".into())));
        assert_eq!(err(" {} "), None);
    }
}
