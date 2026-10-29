//! Ported from `findings.py`: findings from a Ringo review, located in files, for scorers that match per file
//! (ReviewBench).
//!
//! `parse` splits a review into its numbered `### N.` findings the way pipelines' `findings` does (fenced code
//! cannot start one; the Open questions and Not checked sections end them) and locates each one: the
//! `- Location:` line, else the anchors in the heading's trailing parentheses (or its backticked spans), else the
//! first anchor in the body. A finding anchored to several files becomes one entry per file. A file named without
//! a line gets line 1. `anchor` checks the entries against the repository at head with read-only git object reads;
//! `reviewbench` builds the ReviewBench agent-contract JSON.
//!
//! Python's regex semantics are kept: `\w` is `[\p{L}\p{N}_]`, `\s` adds U+001C..U+001F, re.I letters are spelt out
//! with Python's extra equivalences (İ and ı for i, ſ for s), and line numbers are Python ints (any length up to
//! int()'s 4300 digits, Unicode decimal digits included).

use crate::rt::{Error, Exc, Result, context, pipelines, py};
use fancy_regex::Regex as Fancy;
use regex::Regex;
use serde_json::{Map, Number, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

/// Characters per message.
pub const MESSAGE: usize = 2500;
const W: &str = r"\p{L}\p{N}_";
const S: &str = r"\s\x1C-\x1F";

fn seg() -> String {
    format!(r"[{W}.@+\-\[\]]")
}

/// A path ('/' or '\\' separated, cleaned later): segments with a final '.ext' (lower-case led, or one capital
/// such as .R), or at least one '/', or a well-known bare file name. Not inside a longer token, and not followed by
/// more path characters.
pub static FILE: LazyLock<String> = LazyLock::new(|| {
    let seg = seg();
    format!(
        r"(?<![{W}./\\@+\-\[\]])(/?(?:{seg}+[/\\])*{seg}*\.(?:[a-z][A-Za-z0-9]{{0,9}}|[A-Z])|/?(?:{seg}+[/\\])+{seg}+|Dockerfile|Makefile|Gemfile|Rakefile|Procfile)(?![{W}@+\-\[\]/]|\.[{W}])"
    )
});
static LOC: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(r"{}`?(?:(?::L?|#L|:[{S}]*lines?[{S}]+)~?(\d+)(?:[{S}]*[-–][{S}]*L?~?(\d+))?)?", *FILE))
        .expect("a valid pattern")
});
static FIELD_START: LazyLock<Regex> =
    LazyLock::new(|| re(&format!(r"\A([{S}]*)[-*][{S}]*\**[A-Z][{W} -]{{0,30}}\**[{S}]*:")));
static TRAILING: LazyLock<Regex> = LazyLock::new(|| re(&format!(r"\(([^()]*(?:\([^()]*\)[^()]*)*)\)[{S}*.]*\z")));
static TICKS: LazyLock<Regex> = LazyLock::new(|| re(r"`([^`\n]+)`"));
static PROBLEM: LazyLock<Regex> =
    LazyLock::new(|| re(&format!(r"\A([{S}]*)[-*][{S}]*\**{}\**[{S}]*:[{S}]*\**[{S}]*(.*)\z", ignorecase("Problem"))));
static FIELDS: LazyLock<HashMap<&'static str, Regex>> = LazyLock::new(|| {
    ["Location", "severity", "category"]
        .into_iter()
        .map(|name| (name, re(&format!(r"(?m)^[{S}]*[-*][{S}]*\**{}\**[{S}]*:[{S}]*\**[{S}]*(.*?)[{S}]*\**[{S}]*$", ignorecase(name)))))
        .collect()
});
static DECIMAL: LazyLock<Regex> = LazyLock::new(|| re(r"\A\d\z"));

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid pattern")
}

/// A word as Python's re.IGNORECASE matches it: each ASCII letter by both cases, plus İ and ı for i, ſ for s and
/// the Kelvin sign for k.
fn ignorecase(word: &str) -> String {
    word.chars()
        .map(|c| {
            let (lo, up) = (c.to_ascii_lowercase(), c.to_ascii_uppercase());
            match lo {
                'i' => "[iI\u{130}\u{131}]".to_string(),
                's' => "[sS\u{17F}]".to_string(),
                'k' => "[kK\u{212A}]".to_string(),
                _ if lo.is_ascii_alphabetic() => format!("[{lo}{up}]"),
                _ => regex::escape(&c.to_string()),
            }
        })
        .collect()
}

/// A Python int's decimal digits without leading zeros, for a run of Unicode decimal digits (`int(text)`), or the
/// ValueError int() raises past 4300 digits.
fn int_digits(text: &str) -> Result<String> {
    let count = text.chars().count();
    if count > 4300 {
        return Err(Error::value(format!(
            "Exceeds the limit (4300 digits) for integer string conversion: value has {count} digits; use \
             sys.set_int_max_str_digits() to increase the limit"
        )));
    }
    let ascii: String = text.chars().map(decimal_value).map(|d| char::from(b'0' + d)).collect();
    let trimmed = ascii.trim_start_matches('0');
    Ok(if trimmed.is_empty() { "0".into() } else { trimmed.into() })
}

/// The value of a Unicode decimal digit: every run of them is made of whole 0..9 sequences, so it is the distance
/// from the run's start, modulo 10.
fn decimal_value(c: char) -> u8 {
    if c.is_ascii_digit() {
        return c as u8 - b'0';
    }
    let mut start = c as u32;
    while let Some(prev) = start.checked_sub(1).and_then(char::from_u32) {
        if !DECIMAL.is_match(prev.encode_utf8(&mut [0; 4])) {
            break;
        }
        start -= 1;
    }
    ((c as u32 - start) % 10) as u8
}

/// A non-negative Python int kept as its digits (no leading zeros), ordered by value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line(String);

impl Line {
    pub fn new(n: u64) -> Line {
        Line(n.to_string())
    }
    fn key(&self) -> (usize, &str) {
        (self.0.len(), &self.0)
    }
    fn max(self, other: Line) -> Line {
        if other.key() > self.key() { other } else { self }
    }
    fn min(self, other: Line) -> Line {
        if other.key() < self.key() { other } else { self }
    }
    pub fn to_value(&self) -> Value {
        Value::Number(self.0.parse::<Number>().expect("decimal digits"))
    }
    pub fn from_value(v: &Value) -> Option<Line> {
        let s = v.as_number()?.as_str();
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| Line(s.trim_start_matches('0').to_string()).nonempty())
    }
    fn nonempty(self) -> Line {
        if self.0.is_empty() { Line("0".into()) } else { self }
    }
}

/// A repository-relative path: forward slashes, no leading './' or '/'.
pub fn clean(path: &str) -> String {
    let mut path = path.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_string();
    }
    path.trim_start_matches('/').to_string()
}

/// Findings entries: {file, start_line, end_line, message, finding, ...} objects.
pub type Entries = Vec<Map<String, Value>>;

/// One anchor: (file, start, end).
pub type Anchor = (String, Line, Line);

/// [(file, start, end)] anchors in text, first one per file; a file without a line gets line 1. need_line keeps
/// only anchors with a line number or a directory, which prose mentions such as `d.db` rarely have.
pub fn locations(text: &str, need_line: bool) -> Result<Vec<Anchor>> {
    let (mut out, mut seen) = (Vec::new(), HashSet::new());
    for m in LOC.captures_iter(text) {
        let m = m.map_err(|e| Error::value(e.to_string()))?;
        let path = clean(m.get(1).map_or("", |g| g.as_str()));
        let (g2, g3) = (m.get(2).map(|g| g.as_str()), m.get(3).map(|g| g.as_str()));
        if path.is_empty() || seen.contains(&path) || (need_line && g2.is_none() && !path.contains('/')) {
            continue;
        }
        seen.insert(path.clone());
        let start = Line::new(1).max(match g2 {
            Some(d) => Line(int_digits(d)?),
            None => Line::new(1),
        });
        let end = match g3 {
            Some(d) => start.clone().max(Line(int_digits(d)?)),
            None => start.clone(),
        };
        out.push((path, start, end));
    }
    Ok(out)
}

/// Anchors in a heading: those in its trailing parentheses, else backticked spans in it that have a line or a
/// directory (a bare `name.ext` in a title is as often code, such as `d.db`, as a file).
pub fn heading_anchors(title: &str) -> Result<Vec<Anchor>> {
    let found = match TRAILING.captures(title) {
        Some(m) => locations(m.get(1).map_or("", |g| g.as_str()), false)?,
        None => Vec::new(),
    };
    if !found.is_empty() {
        return Ok(found);
    }
    let spans: Vec<&str> = TICKS.captures_iter(title).filter_map(|c| c.get(1).map(|g| g.as_str())).collect();
    locations(&spans.join(" "), true)
}

/// The value of the `- <name>:` bullet in body (re.M | re.I), or None when there is none or it is empty.
pub fn field<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    let pattern = FIELDS.get(name).expect("a known field");
    pattern.captures(body).and_then(|c| c.get(1)).map(|g| g.as_str()).filter(|s| !s.is_empty())
}

fn indent_of(line: &str) -> usize {
    py::len(line) - py::len(py::lstrip(line))
}

/// The text of the `- Problem:` bullet with its wrapped and nested lines, or None.
pub fn problem(lines: &[&str]) -> Option<String> {
    for (k, line) in lines.iter().enumerate() {
        let Some(m) = PROBLEM.captures(line) else { continue };
        let indent = py::len(m.get(1).map_or("", |g| g.as_str()));
        let mut out: Vec<&str> = vec![m.get(2).map_or("", |g| g.as_str())];
        for nxt in &lines[k + 1..] {
            if !py::strip(nxt).is_empty() {
                let depth = indent_of(nxt);
                if depth <= indent && (FIELD_START.is_match(nxt) || py::lstrip(nxt).starts_with('#')) {
                    break;
                }
                if depth <= indent && py::strip(out.last().expect("one line")).is_empty() {
                    break; // a paragraph after a blank line ends the bullet
                }
            }
            out.push(nxt);
        }
        let text = py::split(&out.join(" ")).join(" ");
        return (!text.is_empty()).then_some(text);
    }
    None
}

/// [{file, start_line, end_line, message, finding, severity?, category?}] for a Ringo review; `finding` is the
/// finding's number in the review, shared by the entries of a finding anchored to several files. A finding with
/// no file anchor gives no entry.
pub fn parse(review_text: &str) -> Result<Entries> {
    let mut out = Vec::new();
    let (_, found, _) = pipelines::findings("f", review_text);
    for (n, body) in found.values().enumerate() {
        let (head, rest) = body.split_once('\n').unwrap_or((body, ""));
        let caps = pipelines::head_match(head).expect("a finding starts with its heading");
        let title = caps.get(2).map_or("", |g| g.as_str()).trim_matches([' ', '*']);
        let lines = py::splitlines(rest, false);
        let mut anchors = match field(rest, "Location") {
            Some(loc) => locations(loc, false)?,
            None => Vec::new(),
        };
        if anchors.is_empty() {
            anchors = heading_anchors(title)?;
        }
        if anchors.is_empty() {
            let mut body_anchors = locations(rest, true)?;
            if body_anchors.is_empty() {
                body_anchors = locations(rest, false)?;
            }
            body_anchors.truncate(1);
            anchors = body_anchors;
        }
        let detail = problem(&lines).unwrap_or_else(|| py::split(rest).join(" "));
        let lead = title.trim_end_matches([' ', '.']);
        let full = if detail.is_empty() { lead.to_string() } else { format!("{lead}. {detail}") };
        let message = py::head(&full, MESSAGE).to_string();
        let mut extra = Map::new();
        for key in ["severity", "category"] {
            if let Some(v) = field(rest, key) {
                extra.insert(key.into(), Value::String(v.to_lowercase()));
            }
        }
        for (f, a, b) in anchors {
            let mut entry = Map::new();
            entry.insert("file".into(), Value::String(f));
            entry.insert("start_line".into(), a.to_value());
            entry.insert("end_line".into(), b.to_value());
            entry.insert("message".into(), Value::String(message.clone()));
            entry.insert("finding".into(), Value::from(n + 1));
            entry.extend(extra.clone());
            out.push(entry);
        }
    }
    Ok(out)
}

/// The file at head that path names: itself, without a diff 'a/' or 'b/' prefix, or the one file whose path ends
/// with '/path' (when several do, the one changed file among them); None when there is none or it stays ambiguous.
pub fn resolve(path: &str, files: &HashSet<String>, changed: &HashSet<String>) -> Option<String> {
    if files.contains(path) {
        return Some(path.to_string());
    }
    if (path.starts_with("a/") || path.starts_with("b/")) && files.contains(&path[2..]) {
        return Some(path[2..].to_string());
    }
    let suffix = format!("/{path}");
    let mut tails: Vec<&String> = files.iter().filter(|f| f.ends_with(&suffix)).collect();
    if tails.len() > 1 {
        tails.retain(|f| changed.contains(*f));
    }
    (tails.len() == 1).then(|| tails[0].clone())
}

fn names(listing: &str) -> HashSet<String> {
    listing.split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect()
}

fn line_of(item: &Map<String, Value>, key: &str) -> Result<Line> {
    let v = item.get(key).ok_or_else(|| Error::new(Exc::Key, py::repr(key)))?;
    Line::from_value(v).ok_or_else(|| Error::new(Exc::Type, format!("{key} must be a non-negative int")))
}

/// (kept, dropped): the entries whose file exists at head, with the path as it is there and lines clamped to the
/// file (a binary file, submodule or empty file has line 1 only), and the entries dropped for a missing file. With
/// base, a bare name shared by several files resolves to the one changed in base...head. Read-only git object
/// reads; ContextError (a ValueError) if head or base is not a commit.
pub fn anchor(
    found: &[Map<String, Value>],
    repo: &Path,
    head: &str,
    base: Option<&str>,
) -> Result<(Entries, Entries)> {
    let head = context::commit(repo, head, "head")?;
    let files = names(&context::git(repo, &["ls-tree", "-r", "-z", "--name-only", &head])?);
    let changed = match base.filter(|b| !b.is_empty()) {
        Some(b) => {
            let range = format!("{}...{head}", context::commit(repo, b, "base")?);
            names(&context::git(repo, &["diff", "--no-ext-diff", "--name-only", "-z", &range])?)
        }
        None => HashSet::new(),
    };
    let (mut lengths, mut kept, mut dropped): (HashMap<String, Line>, Vec<_>, Vec<_>) = (HashMap::new(), vec![], vec![]);
    for item in found {
        let file = item.get("file").and_then(Value::as_str).ok_or_else(|| Error::new(Exc::Key, "'file'"))?;
        let Some(path) = resolve(&clean(file), &files, &changed) else {
            dropped.push(item.clone());
            continue;
        };
        if !lengths.contains_key(&path) {
            let text = match context::blob(repo, &head, &path, None) {
                Ok(t) => t,
                Err(e) if e.is(Exc::Context) => None,
                Err(e) => return Err(e),
            };
            let n = match text.as_deref() {
                Some(t) if !t.is_empty() => (t.matches('\n').count() + usize::from(!t.ends_with('\n'))).max(1),
                _ => 1,
            };
            lengths.insert(path.clone(), Line::new(n as u64));
        }
        let length = lengths[&path].clone();
        let start = Line::new(1).max(line_of(item, "start_line")?).min(length.clone());
        let end = start.clone().max(line_of(item, "end_line")?).min(length);
        let mut entry = item.clone();
        entry.insert("file".into(), Value::String(path));
        entry.insert("start_line".into(), start.to_value());
        entry.insert("end_line".into(), end.to_value());
        kept.push(entry);
    }
    Ok((kept, dropped))
}

/// `str.isdigit()`: a decimal digit, or one of the 128 digits that are not decimal (superscripts, circled ...).
fn isdigit(c: char) -> bool {
    const DIGITS: [(u32, u32); 20] = [
        (0xb2, 0xb3), (0xb9, 0xb9), (0x1369, 0x1371), (0x19da, 0x19da), (0x2070, 0x2070), (0x2074, 0x2079),
        (0x2080, 0x2089), (0x2460, 0x2468), (0x2474, 0x247c), (0x2488, 0x2490), (0x24ea, 0x24ea), (0x24f5, 0x24fd),
        (0x24ff, 0x24ff), (0x2776, 0x277e), (0x2780, 0x2788), (0x278a, 0x2792), (0x10a40, 0x10a43),
        (0x10e60, 0x10e68), (0x11052, 0x1105a), (0x1f100, 0x1f10a),
    ];
    DECIMAL.is_match(c.encode_utf8(&mut [0; 4])) || DIGITS.iter().any(|&(a, b)| (a..=b).contains(&(c as u32)))
}

/// The ReviewBench agent-contract JSON object: {pr: {repo, pr_number, base, head}, agent, findings}. pr holds what
/// is known (pr.json fields; missing ones are None); a numeric pr_number string becomes an int (and, as in Python,
/// a string of digits int() refuses, such as '²', is its ValueError).
pub fn reviewbench(found: &[Map<String, Value>], pr: &Map<String, Value>, agent: &str) -> Result<Value> {
    let mut number = pr.get("pr_number").cloned().unwrap_or(Value::Null);
    if let Value::String(s) = &number
        && !s.is_empty()
        && s.chars().all(isdigit)
    {
        if !s.chars().all(|c| DECIMAL.is_match(c.encode_utf8(&mut [0; 4]))) {
            return Err(Error::value(format!("invalid literal for int() with base 10: {}", py::repr(s))));
        }
        number = Line(int_digits(s)?).to_value();
    }
    let get = |k: &str| pr.get(k).cloned().unwrap_or(Value::Null);
    let mut head = Map::new();
    head.insert("repo".into(), get("repo"));
    head.insert("pr_number".into(), number);
    head.insert("base".into(), get("base"));
    head.insert("head".into(), get("head"));
    let entries = found
        .iter()
        .map(|f| {
            let mut e = Map::new();
            for k in ["file", "start_line", "end_line", "message"] {
                e.insert(k.into(), f.get(k).cloned().ok_or_else(|| Error::new(Exc::Key, py::repr(k)))?);
            }
            e.insert("producer".into(), Value::String(agent.into()));
            Ok(Value::Object(e))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut out = Map::new();
    out.insert("pr".into(), Value::Object(head));
    out.insert("agent".into(), Value::String(agent.into()));
    out.insert("findings".into(), Value::Array(entries));
    Ok(Value::Object(out))
}
