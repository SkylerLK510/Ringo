//! Ported from `pipelines.py`: pipeline plans for the Ringo harness (which passes run, what each one sees, and how
//! the replies become the final result). Pure functions, no I/O apart from reading plan and prompt files; the
//! harness makes the calls. See the Python module's docstring for the plan format.
//!
//! A plan is JSON, kept as an insertion-ordered `serde_json::Value` exactly as Python keeps its dict; `validate`
//! adds each pass's `inputs` (when absent) and `text` the way Python does, so `pyjson::dumps(plan)` equals
//! `json.dumps(plan)`. Python's `PROMPTS` and `PLANS` live next to the module; here they are folders under the Ringo
//! home (`ringo_cli::ringo_home()`): [`prompts_dir`] and [`plans_dir`], and [`load_plan`] takes the home.
//!
//! Regex notes: Python's `\w` is `str.isalnum() or '_'`, which is exactly Unicode L and N ([`WORD`]); its `\s` is
//! `str.isspace()` ([`SPACE`]); its `re.I` lets `i` match U+0130/U+0131 as well, so the case-insensitive words are
//! spelled out as classes. Python's `\b` is written as the character test it stands for.

use crate::rt::{Error, Exc, Result, py, pyjson};
use indexmap::IndexMap;
use regex::{Captures, Regex};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// The replies of the passes that ran so far, by pass id (Python's `replies` dict; its order never reaches output).
pub type Replies = HashMap<String, String>;

pub const REPLIES: [&str; 5] = ["findings", "verdicts", "issues", "proposal", "text"];
pub const PASS_KEYS: [&str; 7] = ["id", "prompt", "reply", "inputs", "batch", "when", "model"];
pub const PLAN_KEYS: [&str; 5] = ["version", "kind", "role", "passes", "prompts_dir"];
/// Per findings reply in a batched plan: its checker makes at most ceil(readers x 50 / batch) calls.
pub const MAX_FINDINGS: usize = 50;
/// Python's `\w` (inside a character class).
pub const WORD: &str = r"\p{L}\p{N}_";
/// Python's `\s` (inside a character class).
pub const SPACE: &str = r"\s\x1C-\x1F";
/// Python's `ID = r'[A-Za-z][\w-]*'`, in Rust regex syntax.
pub const ID: &str = r"[A-Za-z][\p{L}\p{N}_-]*";

const REPLIES_REPR: &str = "('findings', 'verdicts', 'issues', 'proposal', 'text')";

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid pattern")
}

/// `^#{2,4}\s*\**\s*[A-Za-z]{0,2}(\d+)\s*[.):]\s*(.*)$` with re.M: a numbered finding heading.
pub static HEAD: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^#{2,4}[\s\x1C-\x1F]*\**[\s\x1C-\x1F]*[A-Za-z]{0,2}(\d+)[\s\x1C-\x1F]*[.):][\s\x1C-\x1F]*(.*)$"));
/// The "Open questions" / "Not checked" section heading (re.M | re.I).
pub static SECTION: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?m)^#{2,4}[ \t]*\**[ \t]*(",
        r"[oO][pP][eE][nN] [qQ][uU][eE][sS\x{17F}][tT][iI\x{130}\x{131}][oO][nN][sS\x{17F}]",
        r"|[nN][oO][tT] [cC][hH][eE][cC][kK\x{212A}][eE][dD]",
        r")(?:[ \t]*\([^\n)]*\))?[ \t]*\**[ \t]*:?[ \t]*$"
    ))
});
/// `^[^\w\n]*(ID\.\d+)[^\w\n]+(KEEP|REFUTED|DUP|OPEN)\b(.*)$` with re.M; group 3 is Python's `(.*)`.
pub static VERDICT: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?m)^[^\p{L}\p{N}_\n]*([A-Za-z][\p{L}\p{N}_-]*\.\d+)[^\p{L}\p{N}_\n]+(KEEP|REFUTED|DUP|OPEN)((?:[^\p{L}\p{N}_\n].*)?)$")
});
/// `\bID\.\d+`; Python's leading `\b` is matched as a character here, so group 1 is the finding id
/// (use [`finding_id`] for Python's `FINDING_ID.search(text).group(0)`).
pub static FINDING_ID: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?:\A|[^\p{L}\p{N}_])([A-Za-z][\p{L}\p{N}_-]*\.\d+)"));
/// `^[^\w\n]*(ID\.\d+)[^\w\n]+CUT\b[^\w\n"]*"(.+)"[^\w\n]*$` with re.M (the `\b` always holds when the rest matches).
pub static CUT: LazyLock<Regex> = LazyLock::new(|| {
    re(r#"(?m)^[^\p{L}\p{N}_\n]*([A-Za-z][\p{L}\p{N}_-]*\.\d+)[^\p{L}\p{N}_\n]+CUT[^\p{L}\p{N}_\n"]*"(.+)"[^\p{L}\p{N}_\n]*$"#)
});
/// `^\W*no defects found\b` with re.M | re.I. Use it for `is_match` only: the trailing `\b` consumes a character.
pub static NO_DEFECTS: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?m)^[^\p{L}\p{N}_]*[nN][oO] [dD][eE][fF][eE][cC][tT][sS\x{17F}] [fF][oO][uU][nN][dD](?:[^\p{L}\p{N}_]|\z)")
});
static NONE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"\A[^\p{L}\p{N}_\n]*[nN][oO][nN][eE][^\p{L}\p{N}_\n]*(?:[-\x{2013}\x{2014}:] [^\n]*)?\z")
});
static OPENING: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^[ \t]{0,3}(`{3,}|~{3,})[^\n]*$"));
static ID_FULL: LazyLock<Regex> = LazyLock::new(|| re(r"\A[A-Za-z][\p{L}\p{N}_-]*\z"));
static MODEL: LazyLock<Regex> = LazyLock::new(|| re(r"\A[\p{L}\p{N}_.:-]+\z"));
static PROMPT: LazyLock<Regex> = LazyLock::new(|| re(r"\A[\p{L}\p{N}_.-]+\.md\z"));
static NONPRINTABLE: LazyLock<Regex> = LazyLock::new(|| re(r"[\p{Cc}\p{Cf}\p{Co}\p{Cn}\p{Zl}\p{Zp}\p{Zs}]"));

/// Python's `re.fullmatch(ID, s)`.
pub fn is_id(s: &str) -> bool {
    ID_FULL.is_match(s)
}

/// Python's `NO_DEFECTS.search(text)` as a bool.
pub fn no_defects(text: &str) -> bool {
    NO_DEFECTS.is_match(text)
}

/// Python's `FINDING_ID.search(text).group(0)`.
pub fn finding_id(text: &str) -> Option<&str> {
    FINDING_ID.captures(text).map(|c| c.get(1).map_or("", |m| m.as_str()))
}

/// Python's `HEAD.match(text)`: a heading match that starts at the beginning of text.
pub fn head_match(text: &str) -> Option<Captures<'_>> {
    HEAD.captures(text).filter(|c| c.get(0).is_some_and(|m| m.start() == 0))
}

/// Findings that still need a verdict; the harness asks the checker once more. As an [`Error`] it is
/// `Exc::NeedVerdicts` with Python's message `"<reason>: <id>, <id>"` and data `{"ids": [...], "reason": ...}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeedVerdicts {
    pub ids: Vec<String>,
    pub reason: String,
}

impl NeedVerdicts {
    pub fn new(ids: Vec<String>, reason: impl Into<String>) -> NeedVerdicts {
        NeedVerdicts { ids, reason: reason.into() }
    }

    pub fn error(&self) -> Error {
        Error::new(Exc::NeedVerdicts, format!("{}: {}", self.reason, self.ids.join(", ")))
            .with_data(json!({"ids": self.ids, "reason": self.reason}))
    }

    /// The NeedVerdicts an error carries, if it is one.
    pub fn from_error(e: &Error) -> Option<NeedVerdicts> {
        if e.exc != Exc::NeedVerdicts {
            return None;
        }
        let data = e.data.as_ref()?;
        let ids = data.get("ids")?.as_array()?.iter().map(|i| i.as_str().map(str::to_string)).collect::<Option<_>>()?;
        Some(NeedVerdicts { ids, reason: data.get("reason")?.as_str()?.to_string() })
    }
}

impl From<NeedVerdicts> for Error {
    fn from(n: NeedVerdicts) -> Error {
        n.error()
    }
}

/// Python's `PROMPTS`: `<home, resolved>/prompts`.
pub fn prompts_dir(home: &Path) -> PathBuf {
    resolve(home).unwrap_or_else(|_| home.to_path_buf()).join("prompts")
}

/// Python's `PLANS`: `<home, resolved>/plans`.
pub fn plans_dir(home: &Path) -> PathBuf {
    resolve(home).unwrap_or_else(|_| home.to_path_buf()).join("plans")
}

/// The plan in the JSON file at path (default: the built-in `plans/<kind>.json` under home), validated.
/// An empty path counts as none, as Python's `if path` does for ''.
pub fn load_plan(home: &Path, path: Option<&Path>, kind: &str) -> Result<Value> {
    let path = match path {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => plans_dir(home).join(format!("{kind}.json")),
    };
    let text = read_text(&path, &path_str(&path))?;
    let plan = json_loads(&text)?;
    let Value::Object(obj) = &plan else { return Err(Error::value("a plan is a JSON object")) };
    if obj.get("kind").and_then(Value::as_str) != Some(kind) {
        return Err(Error::value(format!("a {kind} task needs a {kind} plan")));
    }
    let base = match obj.get("prompts_dir") {
        Some(Value::String(d)) => resolve(&path.parent().unwrap_or(Path::new("/")).join(d))?,
        _ => prompts_dir(home),
    };
    validate(plan, &base)
}

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object().and_then(|m| m.get(key))
}

fn field<'a>(v: &'a Value, key: &str) -> &'a str {
    get(v, key).and_then(Value::as_str).unwrap_or("")
}

fn passes(plan: &Value) -> &[Value] {
    get(plan, "passes").and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn inputs(p: &Value) -> impl Iterator<Item = &str> {
    get(p, "inputs").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str)
}

/// Python's text of an integer JSON number (no fraction or exponent), as `type(x) is int` tells.
fn int_text(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()).filter(|t| !t.contains(['.', 'e', 'E'])),
        _ => None,
    }
}

/// A positive Python int (`type(v) is int and v >= 1`); one larger than usize saturates.
fn positive(v: &Value) -> Option<usize> {
    let t = int_text(v)?;
    if t.starts_with('-') || t == "0" {
        return None;
    }
    Some(t.parse().unwrap_or(usize::MAX))
}

/// `p.get('batch')` of a validated pass, when set.
fn batch(p: &Value) -> Option<usize> {
    get(p, "batch").and_then(positive)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(m)) => !m.is_empty(),
    }
}

/// Python's `x == 1` for a JSON value (True and 1.0 are equal to 1 too).
fn is_one(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => {
            let t = n.to_string();
            if t.contains(['.', 'e', 'E']) { t.parse::<f64>() == Ok(1.0) } else { t == "1" }
        }
        _ => false,
    }
}

/// The plan with each pass's prompt text loaded; ValueError on anything malformed.
pub fn validate(mut plan: Value, prompts_dir: &Path) -> Result<Value> {
    let Value::Object(obj) = &mut plan else { return Err(not_an_object(&plan)) };
    let mut unknown: Vec<&str> = obj.keys().map(String::as_str).filter(|k| !PLAN_KEYS.contains(k)).collect();
    unknown.sort_unstable();
    let kind = obj.get("kind").and_then(Value::as_str).filter(|k| matches!(*k, "review" | "design")).map(str::to_string);
    let Some(kind) = kind.filter(|_| unknown.is_empty() && is_one(obj.get("version"))) else {
        let keys: Vec<String> = unknown.iter().map(|k| repr_str(k)).collect();
        return Err(Error::value(format!(
            "a plan needs version 1 and kind review or design; unknown keys: [{}]",
            keys.join(", ")
        )));
    };
    if !matches!(obj.get("role"), Some(Value::String(r)) if !py::strip(r).is_empty()) {
        return Err(Error::value("a plan needs a role line"));
    }
    let passes = match obj.get_mut("passes") {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return Err(Error::value("a plan needs at least one pass")),
    };
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut kinds: Vec<String> = Vec::new();
    for p in passes.iter_mut() {
        let Some(m) = p.as_object_mut().filter(|m| m.keys().all(|k| PASS_KEYS.contains(&k.as_str()))) else {
            return Err(Error::value(format!("pass {}: unknown keys", repr(p))));
        };
        let pid = match m.get("id") {
            Some(Value::String(s)) if ID_FULL.is_match(s) && !seen.contains_key(s) => s.clone(),
            other => {
                return Err(Error::value(format!(
                    "pass id {} is missing, malformed or repeated",
                    other.map_or("None".into(), repr)
                )));
            }
        };
        let reply = match m.get("reply") {
            Some(Value::String(s)) if REPLIES.contains(&s.as_str()) => s.clone(),
            _ => return Err(Error::value(format!("pass {pid}: reply must be one of {REPLIES_REPR}"))),
        };
        let given = m.entry("inputs").or_insert_with(|| Value::Array(Vec::new()));
        if !matches!(given, Value::Array(a) if a.iter().all(|i| matches!(i, Value::String(s) if seen.contains_key(s)))) {
            return Err(Error::value(format!("pass {pid}: inputs must name earlier passes")));
        }
        if let Some(w) = m.get("when")
            && (!matches!(w, Value::String(s) if seen.get(s).is_some_and(|r| r == "issues")) || reply == "verdicts")
        {
            return Err(Error::value(format!(
                "pass {pid}: when must name an earlier issues pass; a verdicts pass always runs"
            )));
        }
        if m.get("batch").is_some_and(|b| reply != "verdicts" || positive(b).is_none()) {
            return Err(Error::value(format!("pass {pid}: batch is a positive integer, for verdicts passes only")));
        }
        if let Some(model) = m.get("model")
            && !matches!(model, Value::String(s) if MODEL.is_match(s))
        {
            return Err(Error::value(format!("pass {pid}: model is a model name")));
        }
        let name = match m.get("prompt") {
            Some(Value::String(s)) if PROMPT.is_match(s) => s.clone(),
            _ => return Err(Error::value(format!("pass {pid}: prompt must be a .md file name"))),
        };
        let file = resolve(&prompts_dir.join(&name))?;
        let parent_ok = file.parent().is_some_and(|d| resolve(prompts_dir).is_ok_and(|base| d == base));
        if !parent_ok || !std::fs::metadata(&file).is_ok_and(|md| md.is_file()) {
            return Err(Error::value(format!("pass {pid}: prompt {name} not found in {}", prompts_dir.display())));
        }
        let text = read_text(&file, &file.to_string_lossy())?;
        m.insert("text".into(), Value::String(py::strip(&text).to_string()));
        seen.insert(pid, reply.clone());
        kinds.push(reply);
    }
    let count = |r: &str| kinds.iter().filter(|k| *k == r).count();
    let (readers, checkers) = (count("findings"), count("verdicts"));
    if kind == "review" && (readers == 0 || checkers > 1 || (readers > 1 && checkers == 0)) {
        return Err(Error::value(
            "a review plan needs one or more findings passes, and one verdicts pass when there are several",
        ));
    }
    if kind == "review" && checkers > 0 {
        let first_checker = kinds.iter().position(|k| k == "verdicts").unwrap_or(0);
        let last_reader = kinds.iter().rposition(|k| k == "findings").unwrap_or(0);
        if first_checker < last_reader {
            return Err(Error::value("the verdicts pass must come after every findings pass"));
        }
    }
    let output = if kind == "review" { "findings" } else { "proposal" };
    if !passes.iter().any(|p| field(p, "reply") == output && get(p, "when").is_none()) {
        return Err(Error::value(format!("a {kind} plan needs a {output} pass without when")));
    }
    Ok(plan)
}

/// What Python raises when validate() is handed a JSON value that is not an object. AttributeError has no `Exc`
/// kind and is reported as `Exc::Type`; lists holding non-string scalars sort (or fail to) in ways not mirrored.
fn not_an_object(plan: &Value) -> Error {
    let type_name = match plan {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(_) if int_text(plan).is_some() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    };
    let elements: Vec<String> = match plan {
        Value::String(s) => s.chars().map(String::from).collect(),
        Value::Array(a) => {
            if let Some(bad) = a.iter().find(|v| v.is_array() || v.is_object()) {
                let t = if bad.is_array() { "list" } else { "dict" };
                return Error::new(Exc::Type, format!("cannot use '{t}' as a set element (unhashable type: '{t}')"));
            }
            if !a.iter().all(Value::is_string) {
                return Error::new(Exc::Type, "a plan is a JSON object");
            }
            a.iter().filter_map(Value::as_str).map(str::to_string).collect()
        }
        _ => return Error::new(Exc::Type, format!("'{type_name}' object is not iterable")),
    };
    let mut unknown: Vec<&str> = elements.iter().map(String::as_str).filter(|k| !PLAN_KEYS.contains(k)).collect();
    if unknown.is_empty() {
        return Error::new(Exc::Type, format!("'{type_name}' object has no attribute 'get'"));
    }
    unknown.sort_unstable();
    unknown.dedup();
    let keys: Vec<String> = unknown.iter().map(|k| repr_str(k)).collect();
    Error::value(format!("a plan needs version 1 and kind review or design; unknown keys: [{}]", keys.join(", ")))
}

/// Passes grouped into waves: the first pass alone (it writes the cache), then each set whose inputs are ready.
pub fn waves(plan: &Value) -> Result<Vec<Vec<&Value>>> {
    let all = passes(plan);
    let Some(first) = all.first() else { return Ok(Vec::new()) };
    let first_id = field(first, "id");
    let mut done: HashSet<&str> = HashSet::from([first_id]);
    let mut out = vec![vec![first]];
    let mut rest: Vec<&Value> = all[1..].iter().collect();
    while !rest.is_empty() {
        let ready: Vec<&Value> = rest
            .iter()
            .copied()
            .filter(|p| {
                inputs(p).all(|i| done.contains(i))
                    && done.contains(get(p, "when").map_or(first_id, |w| w.as_str().unwrap_or("")))
                    && !(field(p, "reply") == "verdicts"
                        && rest.iter().any(|q| !std::ptr::eq(*q, *p) && field(q, "reply") == "findings"))
            })
            .collect();
        if ready.is_empty() {
            return Err(Error::value("passes cannot be ordered"));
        }
        done.extend(ready.iter().map(|p| field(p, "id")));
        rest.retain(|p| !done.contains(field(p, "id")));
        out.push(ready);
    }
    Ok(out)
}

type Span = (usize, usize, usize, usize, bool);

/// Fence spans of value: (open start, open end, close start, close end, closed), in byte offsets.
fn spans(value: &str) -> Vec<Span> {
    let mut closers: HashMap<(char, usize), Regex> = HashMap::new();
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(m) = OPENING.captures_at(value, cursor) {
        let (whole, marker) = (m.get(0).expect("group 0"), m.get(1).expect("group 1").as_str());
        let key = (marker.chars().next().unwrap_or('`'), marker.len());
        let close = closers.entry(key).or_insert_with(|| {
            re(&format!(r"(?m)^[ \t]{{0,3}}{}{{{},}}[ \t]*$", regex::escape(&key.0.to_string()), key.1))
        });
        match close.find_at(value, (whole.end() + 1).min(value.len())) {
            Some(end) => {
                out.push((whole.start(), whole.end(), end.start(), end.end(), true));
                cursor = end.end();
            }
            None => {
                out.push((whole.start(), whole.end(), value.len(), value.len(), false));
                break;
            }
        }
    }
    out
}

/// Unwrap one closed whole-reply fence; mask inner fences through their close or EOF. Returns (text, masked):
/// masked has text's code points with every fenced one but `\n` turned into a space.
pub fn unfenced(text: &str) -> (String, String) {
    let mut text = py::strip(text);
    if let Some(&(0, open_end, close_start, close_end, true)) = spans(text).first()
        && close_end == text.len()
    {
        text = text[open_end..close_start].trim_matches('\n');
    }
    let mut masked = String::with_capacity(text.len());
    let mut at = 0;
    for (start, _, _, end, _) in spans(text) {
        masked.push_str(&text[at..start]);
        masked.extend(text[start..end].chars().map(|c| if c == '\n' { '\n' } else { ' ' }));
        at = end;
    }
    masked.push_str(&text[at..]);
    (text.to_string(), masked)
}

/// Byte offsets in masked to byte offsets in text (the same code point index in both).
struct Offsets {
    masked: Vec<usize>,
    text: Vec<usize>,
}

impl Offsets {
    fn new(text: &str, masked: &str) -> Offsets {
        if text.len() == masked.len() {
            return Offsets { masked: Vec::new(), text: Vec::new() };
        }
        let starts = |s: &str| s.char_indices().map(|(i, _)| i).chain([s.len()]).collect();
        Offsets { masked: starts(masked), text: starts(text) }
    }

    fn at(&self, b: usize) -> usize {
        if self.masked.is_empty() {
            return b;
        }
        self.masked.binary_search(&b).map_or(b, |i| self.text[i])
    }
}

fn depth(heading: &str) -> usize {
    heading.len() - heading.trim_start_matches('#').len()
}

/// (preamble, {"<pid>.<n>": finding text}, sections) for a findings reply; n counts headings in order, so the
/// model's own numbering never collides across passes. sections holds the open-questions and not-checked text.
pub fn findings(pid: &str, text: &str) -> (String, IndexMap<String, String>, IndexMap<String, String>) {
    let (text, masked) = unfenced(text);
    let map = Offsets::new(&text, &masked);
    let heads: Vec<(usize, usize)> = HEAD.find_iter(&masked).map(|m| (map.at(m.start()), depth(m.as_str()))).collect();
    let terminal: Vec<(usize, usize, String)> = SECTION
        .captures_iter(&masked)
        .filter_map(|c| {
            let m = c.get(0)?;
            (heads.is_empty() || depth(m.as_str()) <= heads[0].1)
                .then(|| (map.at(m.start()), map.at(m.end()), c.get(1).map_or("", |g| g.as_str()).to_lowercase()))
        })
        .collect();
    let ends: Vec<usize> = terminal.iter().map(|t| t.0).collect();
    let marks: Vec<usize> = heads.iter().map(|h| h.0).filter(|&s| ends.first().is_none_or(|&e| s < e)).collect();
    let mut found = IndexMap::new();
    for (k, &m) in marks.iter().enumerate() {
        let next = marks.get(k + 1).copied().unwrap_or(text.len());
        let stop = ends.iter().copied().filter(|&s| s > m).fold(next, usize::min);
        found.insert(format!("{pid}.{}", k + 1), py::strip(&text[m..stop]).to_string());
    }
    let mut sections = IndexMap::new();
    for (start, end, key) in &terminal {
        let later = ends.iter().chain(&marks).copied().filter(|&s| s > *start);
        let next = later.fold(text.len(), usize::min);
        sections.insert(key.clone(), py::strip(text.get(*end..next).unwrap_or("")).to_string());
    }
    let stop = marks.iter().take(1).chain(&ends).copied().fold(text.len(), usize::min);
    (py::strip(&text[..stop]).to_string(), found, sections)
}

/// Every finding of every findings pass that ran, in source order.
pub fn all_findings(plan: &Value, replies: &Replies) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for p in passes(plan) {
        if field(p, "reply") == "findings"
            && let Some(text) = replies.get(field(p, "id"))
        {
            out.extend(findings(field(p, "id"), text).1);
        }
    }
    out
}

/// Python's `str(v)` for a value that reaches an f-string.
fn py_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(v) => repr(v),
        None => String::new(),
    }
}

/// ValueError if a reply has the wrong shape for its pass (the harness then asks once more). In a plan with a
/// checker, a findings reply must use the headings or say "No defects found"; in a plan with a batched checker it
/// may hold at most MAX_FINDINGS findings.
pub fn check_reply(plan: &Value, p: &Value, text: &str) -> Result<()> {
    let id = py_str(get(p, "id"));
    let reply = field(p, "reply");
    if py::strip(text).is_empty() {
        return Err(Error::value(format!("pass {id}: empty reply")));
    }
    if reply == "verdicts" && !VERDICT.is_match(text) {
        return Err(Error::value(format!("pass {id}: no verdict lines")));
    }
    let all = passes(plan);
    if reply == "findings" && all.iter().any(|q| field(q, "reply") == "verdicts") {
        let masked = unfenced(text).1;
        let headings = HEAD.find_iter(&masked).count();
        if findings(&id, text).1.len() != headings {
            return Err(Error::value(format!("pass {id}: finding headings appear after a terminal section")));
        }
        if headings == 0 && !NO_DEFECTS.is_match(&masked) {
            return Err(Error::value(format!("pass {id}: no finding headings and no \"No defects found\" line")));
        }
        if headings > MAX_FINDINGS && all.iter().any(|q| field(q, "reply") == "verdicts" && truthy(get(q, "batch"))) {
            return Err(Error::value(format!(
                "pass {id}: {headings} findings, more than the {MAX_FINDINGS} a batched checker plan allows per reply"
            )));
        }
    }
    Ok(())
}

/// False when an issues reply says NONE.
pub fn has_issues(text: &str) -> bool {
    !NONE.is_match(py::strip(text))
}

/// Declared input replies; checker findings already travel in its batch listing.
pub fn instruction(plan: &Value, p: &Value, replies: &Replies) -> String {
    let kinds: HashMap<&str, &str> = passes(plan).iter().map(|q| (field(q, "id"), field(q, "reply"))).collect();
    let checker = field(p, "reply") == "verdicts";
    let mut parts = vec![field(p, "text").to_string()];
    for i in inputs(p) {
        if let Some(r) = replies.get(i)
            && !(checker && kinds.get(i) == Some(&"findings"))
        {
            parts.push(format!("=== Reply of pass {i} ===\n{r}"));
        }
    }
    parts.join("\n\n")
}

fn first_line(s: &str) -> &str {
    py::splitlines(s, false).first().copied().unwrap_or("")
}

/// The messages for one pass: one, or one per batch of findings for a batched verdicts pass.
pub fn requests(plan: &Value, p: &Value, replies: &Replies) -> Vec<String> {
    let head = instruction(plan, p, replies);
    if field(p, "reply") != "verdicts" {
        return vec![head];
    }
    let items: Vec<(String, String)> = all_findings(plan, replies).into_iter().collect();
    if items.is_empty() {
        return Vec::new();
    }
    let size = batch(p).unwrap_or(items.len());
    let index: Vec<String> = items
        .iter()
        .map(|(fid, body)| format!("{fid}: {}", first_line(body).trim_start_matches(['#', '*', ' '])))
        .collect();
    let index = index.join("\n");
    items
        .chunks(size)
        .map(|chunk| {
            let listing: Vec<String> = chunk.iter().map(|(fid, body)| format!("[{fid}]\n{body}")).collect();
            let ids: Vec<&str> = chunk.iter().map(|(fid, _)| fid.as_str()).collect();
            format!(
                "{head}\n\nGive a verdict for each finding in this batch: {}.\nEvery finding, for DUP targets:\n{index}\n\n=== Findings in this batch ===\n{}",
                ids.join(", "),
                listing.join("\n\n")
            )
        })
        .collect()
}

/// One verdicts reply from its batch replies, keeping each batch's verdicts for that batch's findings only.
/// texts holds one reply per request; a pass other than verdicts has exactly one (as in Python, none panics).
pub fn combine(plan: &Value, p: &Value, replies: &Replies, texts: &[String]) -> String {
    if field(p, "reply") != "verdicts" {
        return texts[0].clone();
    }
    let items: Vec<String> = all_findings(plan, replies).into_keys().collect();
    let size = batch(p).unwrap_or(if items.is_empty() { 1 } else { items.len() });
    let kept: Vec<String> = texts
        .iter()
        .enumerate()
        .map(|(k, text)| {
            let lo = k.saturating_mul(size).min(items.len());
            let hi = (k + 1).saturating_mul(size).min(items.len());
            scope_verdicts(text, &items[lo..hi.max(lo)])
        })
        .collect();
    kept.join("\n")
}

/// Only the verdict and CUT lines for the findings this request was allowed to judge.
pub fn scope_verdicts(text: &str, ids: &[String]) -> String {
    let lines: Vec<&str> = [&*VERDICT, &*CUT]
        .into_iter()
        .flat_map(|pattern| pattern.captures_iter(text))
        .filter(|c| ids.iter().any(|i| c.get(1).is_some_and(|g| g.as_str() == i)))
        .filter_map(|c| c.get(0).map(|m| m.as_str()))
        .collect();
    lines.join("\n")
}

/// The message asking the checker again for the findings in need.
pub fn repair_request(plan: &Value, p: &Value, replies: &Replies, need: &NeedVerdicts) -> String {
    let items = all_findings(plan, replies);
    let listing: Vec<String> =
        need.ids.iter().filter_map(|fid| items.get(fid).map(|body| format!("[{fid}]\n{body}"))).collect();
    format!(
        "{}\n\nThese findings still need a verdict ({}). Judge each on its own merits; give DUP only for a finding that repeats one you KEEP.\n\n=== Findings ===\n{}",
        instruction(plan, p, replies),
        need.reason,
        listing.join("\n\n")
    )
}

/// The verdict and DUP target for each id (a later line for the same id wins).
pub type Verdicts = IndexMap<String, (String, Option<String>)>;
/// The sentences to cut for each id.
pub type Cuts = IndexMap<String, Vec<String>>;

/// {id: (verdict, dup target)} and {id: [sentences to cut]}; a later line for the same id wins.
pub fn verdicts(text: &str) -> (Verdicts, Cuts) {
    let mut out = Verdicts::new();
    for c in VERDICT.captures_iter(text) {
        let verdict = c[2].to_string();
        let target = if verdict == "DUP" { finding_id(&c[3]).map(str::to_string) } else { None };
        out.insert(c[1].to_string(), (verdict, target));
    }
    let mut cuts = Cuts::new();
    for c in CUT.captures_iter(text) {
        cuts.entry(c[1].to_string()).or_default().push(c[2].to_string());
    }
    (out, cuts)
}

/// Python's `re.finditer(rf'(?<!\S){re.escape(sentence)}(?!\S)', body)` spans.
fn whole_hits(body: &str, sentence: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    if sentence.is_empty() {
        return out; // CUT's `"(.+)"` never yields one
    }
    let mut from = 0;
    while let Some(i) = body[from..].find(sentence).map(|i| i + from) {
        let j = i + sentence.len();
        if body[..i].chars().next_back().is_none_or(py::isspace) && body[j..].chars().next().is_none_or(py::isspace) {
            out.push((i, j));
            from = j;
        } else {
            from = i + body[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out
}

/// The final result text and stats from the replies; `Exc::NeedVerdicts` if the checker must be asked again.
pub fn assemble(plan: &Value, replies: &Replies) -> Result<(String, Value)> {
    let all = passes(plan);
    if field(plan, "kind") == "design" {
        let ran: Vec<&Value> =
            all.iter().filter(|p| field(p, "reply") == "proposal" && replies.contains_key(field(p, "id"))).collect();
        let Some(last) = ran.last() else { return Err(Error::value("no proposal pass ran")) };
        let id = field(last, "id");
        return Ok((py::strip(&replies[id]).to_string(), json!({"selected": id})));
    }
    let readers: Vec<&Value> =
        all.iter().filter(|p| field(p, "reply") == "findings" && replies.contains_key(field(p, "id"))).collect();
    if readers.is_empty() {
        return Err(Error::value("no findings pass ran"));
    }
    let Some(checker) = all.iter().find(|p| field(p, "reply") == "verdicts") else {
        return Ok((py::strip(&replies[field(readers[0], "id")]).to_string(), json!({"kept": null})));
    };
    let mut pre = String::new();
    let mut sections: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut items: IndexMap<String, String> = IndexMap::new();
    for p in &readers {
        let (head, found, secs) = findings(field(p, "id"), &replies[field(p, "id")]);
        if pre.is_empty() {
            let lines: Vec<&str> = py::splitlines(&head, false).into_iter().filter(|l| !NO_DEFECTS.is_match(l)).collect();
            pre = py::strip(&lines.join("\n")).to_string();
        }
        items.extend(found);
        for (k, v) in secs {
            let lines = py::splitlines(&v, false).into_iter().filter(|l| !py::strip(l).is_empty()).map(str::to_string);
            sections.entry(k).or_default().extend(lines);
        }
    }
    let (verdict, cuts) = verdicts(replies.get(field(checker, "id")).map_or("", String::as_str));
    let missing: Vec<String> = items.keys().filter(|f| !verdict.contains_key(*f)).cloned().collect();
    let orphan: Vec<String> = verdict
        .iter()
        .filter(|(f, (v, target))| {
            items.contains_key(*f)
                && v == "DUP"
                && target.as_ref().is_none_or(|t| {
                    !items.contains_key(t) || verdict.get(t).is_none_or(|(tv, _)| tv != "KEEP")
                })
        })
        .map(|(f, _)| f.clone())
        .collect();
    if !missing.is_empty() || !orphan.is_empty() {
        let reasons: Vec<&str> = [(&missing, "no verdict"), (&orphan, "duplicate of a finding that was not kept")]
            .into_iter()
            .filter(|(ids, _)| !ids.is_empty())
            .map(|(_, reason)| reason)
            .collect();
        return Err(NeedVerdicts::new([missing, orphan].concat(), reasons.join(" and ")).error());
    }
    let (mut kept, mut opened, mut uncut) = (Vec::new(), Vec::new(), 0usize);
    for (fid, body) in &items {
        let v = verdict[fid].0.as_str();
        if v == "KEEP" {
            let mut body = body.clone();
            for sentence in cuts.get(fid).into_iter().flatten() {
                let hits = whole_hits(&body, sentence);
                let &[(i, mut j)] = hits.as_slice() else {
                    uncut += 1;
                    continue;
                };
                if i > 0 && body.as_bytes()[i - 1] == b' ' && body.as_bytes().get(j) == Some(&b' ') {
                    j += 1;
                }
                body = format!("{}{}", &body[..i], &body[j..]);
            }
            kept.push(body);
        } else if v == "OPEN" {
            opened.push(match head_match(body) {
                Some(c) => py::strip(c.get(2).map_or("", |g| g.as_str())).to_string(),
                None => first_line(body).to_string(),
            });
        }
    }
    if !kept.is_empty() && NO_DEFECTS.is_match(&pre) {
        let lines: Vec<&str> = py::splitlines(&pre, false).into_iter().filter(|l| !NO_DEFECTS.is_match(l)).collect();
        pre = lines.join("\n");
    }
    let mut out: Vec<String> = if pre.is_empty() { Vec::new() } else { vec![pre, String::new()] };
    for (n, body) in kept.iter().enumerate() {
        let renumbered = HEAD.replacen(body, 1, |c: &Captures| format!("### {}. {}", n + 1, &c[2]));
        out.extend([renumbered.into_owned(), String::new()]);
    }
    if kept.is_empty() {
        out.extend(["No defects found that survived checking.".to_string(), String::new()]);
    }
    let mut questions: Vec<String> = opened.iter().map(|q| format!("- {q}")).collect();
    for line in unique(sections.get("open questions")) {
        if line.trim_matches(['-', ' ']).to_lowercase() != "none" {
            questions.push(if line.starts_with('-') { line.to_string() } else { format!("- {line}") });
        }
    }
    out.push("### Open questions (not findings)".into());
    if questions.is_empty() {
        out.push("none".into());
    }
    out.extend(questions);
    out.extend([String::new(), "### Not checked".into()]);
    let unchecked = unique(sections.get("not checked"));
    if unchecked.is_empty() {
        out.push("n/a".into());
    }
    out.extend(unchecked.into_iter().map(str::to_string));
    let stats = json!({"kept": kept.len(), "open": opened.len(), "findings": items.len(), "cuts_not_found": uncut});
    Ok((py::strip(&out.join("\n")).to_string(), stats))
}

/// `list(dict.fromkeys(lines))`.
fn unique(lines: Option<&Vec<String>>) -> Vec<&str> {
    let mut seen = HashSet::new();
    lines.into_iter().flatten().map(String::as_str).filter(|l| seen.insert(*l)).collect()
}

/// Python's `repr()` of a str: printable text as is, other code points escaped as `str.isprintable()` decides.
fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    let mut buf = [0u8; 4];
    for c in s.chars() {
        let n = c as u32;
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            _ if n < 0x20 || n == 0x7f => out.push_str(&format!("\\x{n:02x}")),
            _ if n < 0x7f || c == ' ' || !NONPRINTABLE.is_match(c.encode_utf8(&mut buf)) => out.push(c),
            _ if n <= 0xff => out.push_str(&format!("\\x{n:02x}")),
            _ if n <= 0xffff => out.push_str(&format!("\\u{n:04x}")),
            _ => out.push_str(&format!("\\U{n:08x}")),
        }
    }
    out.push(quote);
    out
}

/// Python's `repr()` of the value json.loads makes of v.
fn repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => match int_text(v) {
            Some(t) => if t == "-0" { "0".into() } else { t },
            None => match n.to_string().parse::<f64>() {
                Ok(f) if f.is_infinite() => if f > 0.0 { "inf" } else { "-inf" }.into(),
                Ok(f) => pyjson::float(f),
                Err(_) => n.to_string(),
            },
        },
        Value::String(s) => repr_str(s),
        Value::Array(a) => format!("[{}]", a.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Object(m) => {
            let items: Vec<String> = m.iter().map(|(k, v)| format!("{}: {}", repr_str(k), repr(v))).collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// `str(Path(p))`: Python's normalized spelling of a POSIX path.
fn path_str(p: &Path) -> String {
    let s = p.to_string_lossy();
    let root = if s.starts_with("//") && !s.starts_with("///") {
        "//"
    } else if s.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = s.split('/').filter(|x| !x.is_empty() && *x != ".").collect();
    let out = format!("{root}{}", parts.join("/"));
    if out.is_empty() { ".".into() } else { out }
}

/// `Path(p).resolve()` (non-strict `os.path.realpath`, Python 3.14): symlinks resolved as far as they exist,
/// `..` applied after each resolution, missing parts kept, OS errors ignored.
#[cfg(unix)]
fn resolve(p: &Path) -> Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let filename = p.as_os_str().as_bytes();
    let mut rest: Vec<Option<Vec<u8>>> = filename.split(|&b| b == b'/').rev().map(|s| Some(s.to_vec())).collect();
    let mut part_count = rest.len();
    let mut path: Vec<u8> =
        if filename.starts_with(b"/") { b"/".to_vec() } else { std::env::current_dir()?.into_os_string().into_vec() };
    let mut seen: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
    while part_count > 0 {
        let Some(entry) = rest.pop() else { break };
        let Some(name) = entry else {
            // A resolved symlink marker: the link below it now resolves to path.
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
        let as_path = Path::new(std::ffi::OsStr::from_bytes(&newpath));
        let is_link = std::fs::symlink_metadata(as_path).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            path = newpath;
            continue;
        }
        if let Some(known) = seen.get(&newpath) {
            path = known.clone().unwrap_or(newpath);
            continue;
        }
        let Ok(target) = std::fs::read_link(as_path) else {
            path = newpath;
            continue;
        };
        let target = target.into_os_string().into_vec();
        if target.starts_with(b"/") {
            path = b"/".to_vec();
        }
        seen.insert(newpath.clone(), None);
        rest.push(Some(newpath));
        rest.push(None);
        let parts: Vec<Option<Vec<u8>>> = target.split(|&b| b == b'/').rev().map(|s| Some(s.to_vec())).collect();
        part_count += parts.len();
        rest.extend(parts);
    }
    Ok(PathBuf::from(OsString::from_vec(path)))
}

#[cfg(not(unix))]
fn resolve(_: &Path) -> Result<PathBuf> {
    Err(Error::os("the native runtime runs on macOS and Linux; on Windows ringo uses the Python runtime"))
}

/// `path.read_text(encoding='utf-8')`: Python's OSError and UnicodeDecodeError texts, universal newlines.
fn read_text(path: &Path, shown: &str) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|e| Error::os(py::os_error(&e, Some(shown))))?;
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            let (b, start) = (e.as_bytes(), e.utf8_error().valid_up_to());
            let (end, reason) = match e.utf8_error().error_len() {
                None => (b.len(), "unexpected end of data"),
                Some(n) if matches!(b[start], 0x80..=0xC1 | 0xF5..=0xFF) => (start + n, "invalid start byte"),
                Some(n) => (start + n, "invalid continuation byte"),
            };
            return Err(Error::value(if end == start + 1 {
                format!("'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}", b[start])
            } else {
                format!("'utf-8' codec can't decode bytes in position {start}-{}: {reason}", end - 1)
            }));
        }
    };
    Ok(if text.contains('\r') { text.replace("\r\n", "\n").replace('\r', "\n") } else { text })
}

/// `json.loads(text)` with Python's error texts (JSONDecodeError and the int digit limit). Documents Python
/// accepts that serde cannot hold (NaN, Infinity, lone surrogates, nesting past 128) fail with serde's text.
fn json_loads(text: &str) -> Result<Value> {
    if let Some(msg) = json_error(text) {
        return Err(Error::value(msg));
    }
    pyjson::loads(text)
}

enum Fail {
    /// StopIteration(idx): "Expecting value".
    Stop(usize),
    At(&'static str, usize),
    Raw(String),
}

/// What json.loads raises for text (CPython's C scanner), or None when it parses.
fn json_error(text: &str) -> Option<String> {
    let s: Vec<char> = text.chars().collect();
    let at = |msg: &str, pos: usize| {
        let before = &s[..pos.min(s.len())];
        let line = before.iter().filter(|&&c| c == '\n').count() + 1;
        let col = before.iter().rposition(|&c| c == '\n').map_or(pos + 1, |i| pos - i);
        format!("{msg}: line {line} column {col} (char {pos})")
    };
    if s.first() == Some(&'\u{feff}') {
        return Some(at("Unexpected UTF-8 BOM (decode using utf-8-sig)", 0));
    }
    match scan(&s, ws(&s, 0), 0) {
        Err(Fail::Stop(i)) => Some(at("Expecting value", i)),
        Err(Fail::At(msg, i)) => Some(at(msg, i)),
        Err(Fail::Raw(msg)) => Some(msg),
        Ok(end) => {
            let end = ws(&s, end);
            (end != s.len()).then(|| at("Extra data", end))
        }
    }
}

fn ws(s: &[char], mut i: usize) -> usize {
    while i < s.len() && matches!(s[i], ' ' | '\t' | '\n' | '\r') {
        i += 1;
    }
    i
}

fn starts(s: &[char], i: usize, word: &str) -> bool {
    let w: Vec<char> = word.chars().collect();
    s.get(i..i + w.len()) == Some(w.as_slice())
}

fn scan(s: &[char], idx: usize, depth: usize) -> std::result::Result<usize, Fail> {
    if idx >= s.len() {
        return Err(Fail::Stop(idx));
    }
    if depth > 2000 {
        return Err(Fail::Raw("maximum recursion depth exceeded while decoding a JSON document".into()));
    }
    match s[idx] {
        '"' => json_string(s, idx + 1),
        '{' => json_object(s, idx + 1, depth + 1),
        '[' => json_array(s, idx + 1, depth + 1),
        'n' if starts(s, idx, "null") => Ok(idx + 4),
        't' if starts(s, idx, "true") => Ok(idx + 4),
        'f' if starts(s, idx, "false") => Ok(idx + 5),
        'N' if starts(s, idx, "NaN") => Ok(idx + 3),
        'I' if starts(s, idx, "Infinity") => Ok(idx + 8),
        '-' if starts(s, idx, "-Infinity") => Ok(idx + 9),
        _ => json_number(s, idx),
    }
}

fn json_number(s: &[char], start: usize) -> std::result::Result<usize, Fail> {
    let digit = |i: usize| s.get(i).is_some_and(char::is_ascii_digit);
    let len = s.len();
    let mut idx = start;
    if s[idx] == '-' {
        idx += 1;
        if idx >= len {
            return Err(Fail::Stop(start));
        }
    }
    if ('1'..='9').contains(&s[idx]) {
        idx += 1;
        while digit(idx) {
            idx += 1;
        }
    } else if s[idx] == '0' {
        idx += 1;
    } else {
        return Err(Fail::Stop(start));
    }
    let mut is_float = false;
    if idx + 1 < len && s[idx] == '.' && digit(idx + 1) {
        is_float = true;
        idx += 2;
        while digit(idx) {
            idx += 1;
        }
    }
    if idx + 1 < len && matches!(s[idx], 'e' | 'E') {
        let e_start = idx;
        idx += 1;
        if idx + 1 < len && matches!(s[idx], '-' | '+') {
            idx += 1;
        }
        while digit(idx) {
            idx += 1;
        }
        if digit(idx - 1) {
            is_float = true;
        } else {
            idx = e_start;
        }
    }
    let digits = idx - start - usize::from(s[start] == '-');
    if !is_float && digits > 4300 {
        return Err(Fail::Raw(format!(
            "Exceeds the limit (4300 digits) for integer string conversion: value has {digits} digits; use sys.set_int_max_str_digits() to increase the limit"
        )));
    }
    Ok(idx)
}

fn hex4(s: &[char]) -> Option<u32> {
    s.iter().try_fold(0u32, |acc, c| c.to_digit(16).map(|d| acc * 16 + d))
}

fn json_string(s: &[char], mut end: usize) -> std::result::Result<usize, Fail> {
    let len = s.len();
    let begin = end - 1;
    loop {
        let mut next = end;
        while next < len && s[next] != '"' && s[next] != '\\' {
            if (s[next] as u32) <= 0x1f {
                return Err(Fail::At("Invalid control character at", next));
            }
            next += 1;
        }
        if next == len {
            return Err(Fail::At("Unterminated string starting at", begin));
        }
        if s[next] == '"' {
            return Ok(next + 1);
        }
        next += 1;
        if next == len {
            return Err(Fail::At("Unterminated string starting at", begin));
        }
        if s[next] != 'u' {
            end = next + 1;
            if !matches!(s[next], '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') {
                return Err(Fail::At("Invalid \\escape", end - 2));
            }
            continue;
        }
        next += 1;
        end = next + 4;
        if end > len {
            return Err(Fail::At("Invalid \\uXXXX escape", next - 1));
        }
        let Some(c) = hex4(&s[next..end]) else { return Err(Fail::At("Invalid \\uXXXX escape", end - 5)) };
        if (0xd800..=0xdbff).contains(&c) && end + 6 < len && s[end] == '\\' && s[end + 1] == 'u' {
            let end2 = end + 6;
            let Some(c2) = hex4(&s[end + 2..end2]) else { return Err(Fail::At("Invalid \\uXXXX escape", end2 - 5)) };
            if (0xdc00..=0xdfff).contains(&c2) {
                end = end2;
            }
        }
    }
}

fn json_object(s: &[char], idx: usize, depth: usize) -> std::result::Result<usize, Fail> {
    let len = s.len();
    let mut idx = ws(s, idx);
    if idx < len && s[idx] == '}' {
        return Ok(idx + 1);
    }
    loop {
        if idx >= len || s[idx] != '"' {
            return Err(Fail::At("Expecting property name enclosed in double quotes", idx));
        }
        idx = ws(s, json_string(s, idx + 1)?);
        if idx >= len || s[idx] != ':' {
            return Err(Fail::At("Expecting ':' delimiter", idx));
        }
        idx = ws(s, scan(s, ws(s, idx + 1), depth)?);
        if idx < len && s[idx] == '}' {
            return Ok(idx + 1);
        }
        if idx >= len || s[idx] != ',' {
            return Err(Fail::At("Expecting ',' delimiter", idx));
        }
        let comma = idx;
        idx = ws(s, idx + 1);
        if idx < len && s[idx] == '}' {
            return Err(Fail::At("Illegal trailing comma before end of object", comma));
        }
    }
}

fn json_array(s: &[char], idx: usize, depth: usize) -> std::result::Result<usize, Fail> {
    let len = s.len();
    let mut idx = ws(s, idx);
    if idx < len && s[idx] == ']' {
        return Ok(idx + 1);
    }
    loop {
        idx = ws(s, scan(s, idx, depth)?);
        if idx < len && s[idx] == ']' {
            return Ok(idx + 1);
        }
        if idx >= len || s[idx] != ',' {
            return Err(Fail::At("Expecting ',' delimiter", idx));
        }
        let comma = idx;
        idx = ws(s, idx + 1);
        if idx < len && s[idx] == ']' {
            return Err(Fail::At("Illegal trailing comma before end of array", comma));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_reprs_and_paths() {
        assert_eq!(repr(&json!({"id": "a'b", "n": [1, 2.5, true, null], "x": "\u{a0}\u{2000}é"})),
                   format!("{{'id': \"a'b\", 'n': [1, 2.5, True, None], 'x': '\\xa0\\{}é'}}", "u2000"));
        assert_eq!(path_str(Path::new("a//b/./c/")), "a/b/c");
        assert_eq!(path_str(Path::new("//a")), "//a");
        assert_eq!(path_str(Path::new("")), ".");
    }

    #[test]
    fn json_errors_read_as_python_writes_them() {
        assert_eq!(json_error("[1,]").as_deref(), Some("Illegal trailing comma before end of array: line 1 column 3 (char 2)"));
        assert_eq!(json_error("{\"a\" 1}").as_deref(), Some("Expecting ':' delimiter: line 1 column 6 (char 5)"));
        assert_eq!(json_error(" {} "), None);
    }
}
