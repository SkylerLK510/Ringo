//! Ported from `backends.py`: backend command presets, isolated subprocess execution and strict reply-object
//! parsing. A model call runs in an empty working folder, with the caller's environment minus the TypeSafe key (and,
//! for subscription calls, minus API keys and endpoint overrides), in a process session of its own that a timeout
//! or a stop kills whole. A Codex child also gets a private CODEX_HOME and HOME holding only a copy of the owner's
//! login (and model cache); a login it refreshed is written back to the owner under a lock, compare-and-swap.
//!
//! The JSON reader is CPython 3.14's `json` C scanner (positions, messages, hooks), because `top_level_objects`
//! charges failed attempts by the error positions it reports.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use crate::rt::{Error, Exc, Result, py, pyjson};
use serde_json::{Map, Number, Value};
use std::cell::Cell;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// `JUDGE_PRESETS`: preset name and command line, in the Python dict's order.
pub const JUDGE_PRESETS: [(&str, &str); 3] = [
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
        // Ringo's calls are single tool-free turns: drop the agent scaffolding Codex adds to each one
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

/// `JUDGE_PRESETS[name]` (None where Python raises KeyError).
pub fn judge_preset(name: &str) -> Option<&'static str> {
    JUDGE_PRESETS.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
}

/// `RAW_PEEK`: bytes of a running call's output read for its soft-limit checks.
pub const RAW_PEEK: usize = 1_000_000;
/// `CODEX_INSTRUCTIONS`: replaces Codex's agent prompt (about 22 KB) for lean calls; Ringo's own instructions
/// travel in the message.
pub const CODEX_INSTRUCTIONS: &str = "Follow the instructions in the user message exactly.\n";
/// `CATALOG_TOOL_FIELDS`: catalog fields that attach the code-mode `exec` tool, the multi-agent tools and their
/// instructions to every request; no config flag removes them in codex-cli 0.164.
pub const CATALOG_TOOL_FIELDS: [&str; 4] = ["tool_mode", "multi_agent_version", "multi_agent_reasoning_effort", "apply_patch_tool_type"];

#[cfg(not(unix))]
const UNSUPPORTED: &str = "the native runtime runs on macOS and Linux; on Windows ringo uses the Python runtime";

// ---------- exceptions ----------

/// `JudgeError(message, fatal)`: the judge could not answer. The message is safe to store; `fatal` (in `data`)
/// marks a failure no later request can escape (a refused key), so the run stops. run_isolated adds `stdout`.
pub fn judge_error(message: impl Into<String>, fatal: bool) -> Error {
    Error::new(Exc::Judge, message).with_data(serde_json::json!({"fatal": fatal}))
}

/// `subprocess.TimeoutExpired(cmd, timeout)`: `str()` is "Command '<repr of cmd>' timed out after <timeout>
/// seconds"; `data` carries `cmd` and `timeout` (run_isolated adds `limit` and `stdout`).
pub fn timeout_expired(cmd: &[String], timeout: f64) -> Error {
    timed_out(Exc::Timeout, cmd, timeout)
}

/// `Stopped(cmd, timeout)`: a call stopped on the person's request after it passed its soft limit. It is a
/// TimeoutExpired (`Exc::Stopped.is(Exc::Timeout)`), so the one-retry rule applies; a call killed at its hard
/// limit is a plain TimeoutExpired with `limit` true.
pub fn stopped(cmd: &[String], timeout: f64) -> Error {
    timed_out(Exc::Stopped, cmd, timeout)
}

fn timed_out(exc: Exc, cmd: &[String], timeout: f64) -> Error {
    let list = cmd.iter().map(|a| py::repr(a)).collect::<Vec<_>>().join(", ");
    Error::new(exc, format!("Command '[{list}]' timed out after {} seconds", pyjson::float(timeout)))
        .with_data(serde_json::json!({"cmd": cmd, "timeout": float_json(timeout)}))
}

fn with_fields(mut e: Error, fields: Value) -> Error {
    let mut data = match e.data.take() {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    if let Value::Object(extra) = fields {
        data.extend(extra);
    }
    e.data = Some(Value::Object(data));
    e
}

/// Python's `round(x, 1)`: correctly rounded, ties to even.
fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

// ---------- file_lock ----------

/// The held lock of `file_lock`; dropping it unlocks.
pub struct FileLock<'a> {
    #[cfg(unix)]
    fd: std::os::fd::BorrowedFd<'a>,
    #[cfg(not(unix))]
    _file: std::marker::PhantomData<&'a File>,
}

/// `file_lock(f, blocking)`: an exclusive lock on the open file f against every other process and open file that
/// locks the same path (POSIX flock), held until the returned guard drops. With blocking false a lock that is
/// already held fails with `ErrorKind::WouldBlock` (Python's BlockingIOError).
pub fn file_lock(f: &File, blocking: bool) -> io::Result<FileLock<'_>> {
    #[cfg(unix)]
    {
        use std::os::fd::{AsFd, AsRawFd};
        let fd = f.as_fd();
        flock(fd.as_raw_fd(), libc::LOCK_EX | if blocking { 0 } else { libc::LOCK_NB })?;
        Ok(FileLock { fd })
    }
    #[cfg(not(unix))]
    {
        let _ = (f, blocking);
        Err(io::Error::other(UNSUPPORTED))
    }
}

impl Drop for FileLock<'_> {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = flock(self.fd.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(unix)]
fn flock(fd: std::os::fd::RawFd, op: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: flock only reads its arguments; fd is open for the caller's borrow.
        if unsafe { libc::flock(fd, op) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

// ---------- spawn and kill_tree ----------

/// A command's environment, in order (Python's env dict).
pub type Env = Vec<(OsString, OsString)>;

/// The `subprocess.Popen` keywords Ringo passes. None inherits, as in Python.
#[derive(Default)]
pub struct Popen {
    pub stdin: Option<Stdio>,
    pub stdout: Option<Stdio>,
    pub stderr: Option<Stdio>,
    pub cwd: Option<PathBuf>,
    pub env: Option<Env>,
}

#[cfg(unix)]
const CHDIR_FAILED: i32 = 1 << 20; // added to errno by the child when chdir, not exec, failed

/// `spawn(argv, **kw)`: the command in a process tree of its own (a new session, as `start_new_session=True`),
/// which kill_tree ends with everything it started. Errors carry Python's text: "[Errno 2] No such file or
/// directory: 'codex'" (the cwd's name when changing into it failed), "embedded null byte".
pub fn spawn(argv: &[String], kw: Popen) -> Result<Child> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        use std::os::unix::process::CommandExt;
        if kw.env.iter().flatten().any(|(k, _)| k.as_bytes().contains(&b'=')) {
            return Err(Error::value("illegal environment variable name"));
        }
        let program = argv.first().ok_or_else(|| Error::value("list index out of range"))?;
        let nul = argv.iter().any(|a| a.contains('\0'))
            || kw.env.iter().flatten().any(|(k, v)| k.as_bytes().contains(&0) || v.as_bytes().contains(&0))
            || kw.cwd.as_ref().is_some_and(|c| c.as_os_str().as_bytes().contains(&0));
        if nul {
            return Err(Error::value("embedded null byte"));
        }
        let mut cmd = std::process::Command::new(program);
        cmd.args(&argv[1..]);
        if let Some(env) = kw.env {
            cmd.env_clear().envs(env);
        }
        for (slot, stdio) in [(0, kw.stdin), (1, kw.stdout), (2, kw.stderr)] {
            if let Some(s) = stdio {
                match slot {
                    0 => cmd.stdin(s),
                    1 => cmd.stdout(s),
                    _ => cmd.stderr(s),
                };
            }
        }
        let shown = kw.cwd.as_ref().map(|c| c.to_string_lossy().into_owned());
        let cwd = kw.cwd.map(|c| CString::new(c.into_os_string().into_vec()).expect("checked for NUL above"));
        // Python's child changes into cwd, then calls setsid, then execs (searching the child's PATH).
        // SAFETY: the closure runs between fork and exec and calls only chdir and setsid, both async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                if let Some(dir) = &cwd
                    && libc::chdir(dir.as_ptr()) != 0
                {
                    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL);
                    return Err(io::Error::from_raw_os_error(errno + CHDIR_FAILED));
                }
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().map_err(|e| match e.raw_os_error() {
            Some(code) if code >= CHDIR_FAILED => {
                Error::os(py::os_error(&io::Error::from_raw_os_error(code - CHDIR_FAILED), shown.as_deref()))
            }
            _ => Error::os(py::os_error(&e, Some(program))),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (argv, kw);
        Err(Error::os(UNSUPPORTED))
    }
}

/// `kill_tree(proc)`: SIGKILL to proc's process group (proc and everything it started, see spawn), without
/// reaping proc. Fails as `os.killpg` does: ESRCH (ProcessLookupError) when the group is gone, EPERM
/// (PermissionError) on macOS for a group that holds only zombies.
pub fn kill_tree(proc: &Child) -> io::Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: killpg only sends a signal.
        if unsafe { libc::killpg(proc.id() as libc::pid_t, libc::SIGKILL) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = proc;
        Err(io::Error::other(UNSUPPORTED))
    }
}

// ---------- strict JSON ----------

/// `_no_constant(name)`: a parse_constant hook that refuses NaN, Infinity and -Infinity.
pub fn no_constant(name: &str) -> Result<f64> {
    Err(Error::value(format!("{name} is not a JSON number")))
}

/// `_finite_float(text)`: a parse_float hook that refuses a number that overflows to infinity.
pub fn finite_float(text: &str) -> Result<f64> {
    let value = py_float(text)?;
    if !value.is_finite() {
        return Err(Error::value(format!("{text} is not a finite number")));
    }
    Ok(value)
}

/// Python's `float(text)` for the number texts JSON hooks receive (and Python's strip, sign, inf/nan words and
/// digit underscores).
fn py_float(text: &str) -> Result<f64> {
    let t = py::strip(text);
    let bad = || Error::value(format!("could not convert string to float: {}", py::repr(text)));
    let cleaned: String = if t.contains('_') {
        let b: Vec<char> = t.chars().collect();
        let ok = b.iter().enumerate().all(|(i, &c)| {
            c != '_' || (i > 0 && i + 1 < b.len() && b[i - 1].is_ascii_digit() && b[i + 1].is_ascii_digit())
        });
        if !ok {
            return Err(bad());
        }
        b.into_iter().filter(|&c| c != '_').collect()
    } else {
        t.to_string()
    };
    if cleaned.is_empty() || !cleaned.is_ascii() {
        return Err(bad());
    }
    cleaned.parse::<f64>().map_err(|_| bad())
}

/// A float as the JSON value Ringo keeps for it: infinities as a number that pyjson writes as Python does
/// (`Infinity`); NaN, which serde_json cannot hold, as null.
fn float_json(f: f64) -> Value {
    if let Some(n) = Number::from_f64(f) {
        return Value::Number(n);
    }
    if f.is_infinite() {
        let text = if f > 0.0 { "1e999" } else { "-1e999" };
        if let Ok(n) = text.parse::<Number>() {
            return Value::Number(n);
        }
    }
    Value::Null
}

/// `_Object`: an object `top_level_objects` found, and whether its text repeats a key (`repeats_a_key`). Only the
/// top-level object's flag is kept; nested objects are plain maps.
#[derive(Clone, Debug, PartialEq)]
pub struct Object {
    pub map: Map<String, Value>,
    pub repeats_a_key: bool,
}

/// `_object(pairs)`: the dict of the pairs (a repeated key keeps its first position and its last value).
pub fn object(pairs: Vec<(String, Value)>) -> Object {
    let n = pairs.len();
    let mut map = Map::new();
    for (k, v) in pairs {
        map.insert(k, v);
    }
    Object { repeats_a_key: map.len() < n, map }
}

/// `OBJECT_START`: where an object can start under json's own grammar: '{', its whitespace, then a key or '}'.
/// Any other '{' fails at once, but building that failure's JSONDecodeError counts lines up to it, so prose full
/// of braces cost time quadratic in its length.
pub const OBJECT_START: &str = r#"\{[ \t\n\r]*["}]"#;
/// `FAILED_PARSE`: failed attempts may parse this many times the text's length in all
pub const FAILED_PARSE: i64 = 64;
/// `FAILED_STARTS`: and number at most this many; each builds an error message that counts lines up to it
pub const FAILED_STARTS: i64 = 1_000;

/// `OBJECT_START.search(text, start)` on code points.
fn object_start(s: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i < s.len() {
        if s[i] == '{' {
            let j = skip_ws(s, i + 1);
            if j < s.len() && (s[j] == '"' || s[j] == '}') {
                return Some(i);
            }
            i = j;
            continue;
        }
        i += 1;
    }
    None
}

/// `top_level_objects(text)`: every top-level JSON object in text, bare or inside prose or a code fence. An object
/// nested in another is never returned on its own, and each records whether it repeats a key. Linear in the text's
/// length: a '{' that cannot start an object is skipped without parsing, nesting too deep to parse is a failed
/// attempt, and failed attempts are bounded in the distance they parse (FAILED_PARSE times the length) and in number
/// (FAILED_STARTS); past either bound the scan ends. Objects holding NaN, Infinity or a number that overflows are
/// refused. Python's generator raises (here: Err) when an integer has more than 4300 digits.
pub fn top_level_objects(text: &str) -> Result<Vec<Object>> {
    let s: Vec<char> = text.chars().collect();
    let refused = Cell::new(false); // NaN, Infinity or a number that overflows: noted, so the attempt ends at a known position
    let repeats = Cell::new(false); // the outermost object's pairs arrive last
    let constant = |_: &str| -> Result<Value> {
        refused.set(true);
        Ok(float_json(0.0))
    };
    let number = |token: &str| -> Result<Value> {
        let value = py_float(token)?;
        if !value.is_finite() {
            refused.set(true);
        }
        Ok(float_json(value))
    };
    let pairs = |pairs: Vec<(String, Value)>| -> Result<Value> {
        let obj = object(pairs);
        repeats.set(obj.repeats_a_key);
        Ok(Value::Object(obj.map))
    };
    let decoder = Decoder { parse_constant: &constant, parse_float: &number, object_pairs_hook: &pairs, max_depth: MAX_DEPTH };
    let (mut parse, mut starts, mut start) = (FAILED_PARSE * (s.len() as i64 + 1), FAILED_STARTS, 0);
    let mut found = Vec::new();
    while let Some(at) = object_start(&s, start) {
        refused.set(false);
        match decoder.raw_decode(&s, at) {
            Err(JsonFail::Decode { pos, .. }) => parse -= (pos - at) as i64,
            Err(JsonFail::Recursion { .. }) => parse -= (s.len() - at) as i64,
            Err(JsonFail::Raised(e)) => return Err(e),
            Ok((obj, end)) => {
                if !refused.get() {
                    start = end; // skip the object's inside: a nested object is never the answer
                    if let Value::Object(map) = obj {
                        found.push(Object { map, repeats_a_key: repeats.get() });
                    }
                    continue;
                }
                parse -= (end - at) as i64; // it parsed, but a refused number makes it no answer
            }
        }
        starts -= 1;
        if parse < 0 || starts < 0 {
            break;
        }
        start = at + 1;
    }
    Ok(found)
}

/// Nesting levels CPython 3.14 parses on this machine's main thread before its stack check raises RecursionError
/// (measured: 116,194 nested objects in `top_level_objects`; the bound moves by tens with the mix of arrays and
/// objects, and with the thread's stack size and the platform). Used where only validity matters (a login file).
pub const PY_MAX_DEPTH: usize = 116_194;
/// Nesting levels a decoded value may have where Rust keeps it: serde_json values are dropped and written
/// recursively, and a few thousand levels overflow a 2 MB thread stack. Deeper input is a RecursionError here,
/// where Python 3.14 still parses it (a reported deviation); 1000 is Python's own default recursion limit.
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
    /// The exception as Python reports it for the document `doc`. RecursionError has no `Exc` kind: it is
    /// reported as `Exc::Type`, which like RecursionError is neither a ValueError nor an OSError.
    pub fn into_error(self, doc: &str) -> Error {
        match self {
            JsonFail::Decode { msg, pos } => Error::value(decode_error_text(msg, doc.chars(), pos)),
            JsonFail::Recursion { what } => recursion_error(what),
            JsonFail::Raised(e) => e,
        }
    }
}

fn recursion_error(what: &str) -> Error {
    Error::new(Exc::Type, format!("maximum recursion depth exceeded while decoding a JSON {what} from a unicode string"))
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
/// it (strict strings, integers through `int()`). Positions are code points. Unpaired surrogate escapes, which a
/// Rust string cannot hold, become U+FFFD. Nesting past `max_depth` is a RecursionError.
pub struct Decoder<'a> {
    pub parse_constant: TextHook<'a>,
    pub parse_float: TextHook<'a>,
    pub object_pairs_hook: PairsHook<'a>,
    pub max_depth: usize,
}

/// The default object hook: a plain dict.
pub fn dict(pairs: Vec<(String, Value)>) -> Result<Value> {
    Ok(Value::Object(object(pairs).map))
}

fn strict_constant(name: &str) -> Result<Value> {
    no_constant(name).map(float_json)
}

fn strict_float(text: &str) -> Result<Value> {
    finite_float(text).map(float_json)
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
                Frame::Array(items) => items.into_iter().for_each(dismantle),
                Frame::Object(pairs, _) => pairs.into_iter().for_each(|(_, v)| dismantle(v)),
            }
        }
    }
}

/// Drops a value of any depth without recursion.
pub fn dismantle(value: Value) {
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

fn skip_ws(s: &[char], mut i: usize) -> usize {
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

impl Decoder<'static> {
    /// `json.loads(text, parse_constant=_no_constant, parse_float=_finite_float)`: plain dicts, no NaN or
    /// infinities.
    pub fn strict() -> Decoder<'static> {
        Decoder { parse_constant: &strict_constant, parse_float: &strict_float, object_pairs_hook: &dict, max_depth: MAX_DEPTH }
    }
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
    /// surrogatepass. A decode failure is a ValueError, as Python's JSONDecodeError and UnicodeDecodeError are;
    /// the UnicodeDecodeError text is not Python's.
    pub fn loads_bytes(&self, data: &[u8]) -> std::result::Result<Value, JsonFail> {
        let s = decode_bytes(data).map_err(JsonFail::Raised)?;
        self.decode(&s).map_err(|e| match e {
            JsonFail::Decode { msg, pos } => JsonFail::Raised(Error::value(decode_error_text(msg, s.iter().copied(), pos))),
            other => other,
        })
    }

    /// `JSONDecoder.decode(s)`: one value, whitespace around it, "Extra data" after it.
    pub fn decode(&self, s: &[char]) -> std::result::Result<Value, JsonFail> {
        let (value, end) = self.raw_decode(s, skip_ws(s, 0))?;
        let end = skip_ws(s, end);
        if end != s.len() {
            return Err(fail("Extra data", end));
        }
        Ok(value)
    }

    /// `JSONDecoder.raw_decode(s, idx)`: the value starting at code point idx and the index after it.
    pub fn raw_decode(&self, s: &[char], mut idx: usize) -> std::result::Result<(Value, usize), JsonFail> {
        let n = s.len();
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
                        let (text, end) = scanstring(s, idx + 1)?;
                        (Value::String(text), end)
                    }
                    '{' => {
                        if stack.len() >= self.max_depth {
                            return Err(JsonFail::Recursion { what: "object" });
                        }
                        let i = skip_ws(s, idx + 1);
                        if i < n && s[i] == '}' {
                            break 'value ((self.object_pairs_hook)(Vec::new()).map_err(JsonFail::Raised)?, i + 1);
                        }
                        let (key, i) = key(s, i)?;
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
                    'N' if idx + 2 < n && starts(s, idx + 1, "aN") => {
                        ((self.parse_constant)("NaN").map_err(JsonFail::Raised)?, idx + 3)
                    }
                    'I' if idx + 7 < n && starts(s, idx + 1, "nfinity") => {
                        ((self.parse_constant)("Infinity").map_err(JsonFail::Raised)?, idx + 8)
                    }
                    '-' if idx + 8 < n && starts(s, idx + 1, "Infinity") => {
                        ((self.parse_constant)("-Infinity").map_err(JsonFail::Raised)?, idx + 9)
                    }
                    _ => self.number(s, idx)?,
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
                    let (k, after) = key(s, j)?;
                    *pending = k;
                    idx = after;
                }
                break;
            }
        }
    }

    /// `_match_number_unicode`: an integer (through `int()`, with its digit limit) or a float (parse_float).
    fn number(&self, s: &[char], start: usize) -> std::result::Result<(Value, usize), JsonFail> {
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
fn key(s: &[char], i: usize) -> std::result::Result<(String, usize), JsonFail> {
    if i >= s.len() || s[i] != '"' {
        return Err(fail("Expecting property name enclosed in double quotes", i));
    }
    let (k, i) = scanstring(s, i + 1)?;
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
fn scanstring(s: &[char], mut end: usize) -> std::result::Result<(String, usize), JsonFail> {
    let (n, begin) = (s.len(), end - 1);
    let mut out = String::new();
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

// ---------- the private Codex home ----------

/// The login file the owner's Codex uses and where it lives: `_codex_home`'s (owner, snapshot, copy).
struct Login {
    owner: PathBuf,
    snapshot: Option<Vec<u8>>,
    copy: PathBuf,
}

/// `str(PurePosixPath(p))`: one root ('/' or exactly '//'), no empty or '.' parts, '.' when nothing is left.
fn pure_path(p: &[u8]) -> Vec<u8> {
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

/// `PurePosixPath(base) / name`.
fn pure_join(base: &[u8], name: &str) -> Vec<u8> {
    let mut p = base.to_vec();
    p.push(b'/');
    p.extend(name.as_bytes());
    pure_path(&p)
}

/// `PurePosixPath(p).parent` of a pure path with at least one part.
fn pure_parent(p: &[u8]) -> Vec<u8> {
    match p.iter().rposition(|&c| c == b'/') {
        None => b".".to_vec(),
        Some(0) => b"/".to_vec(),
        Some(1) if p.starts_with(b"//") => b"//".to_vec(),
        Some(i) => p[..i].to_vec(),
    }
}

/// `pathlib.PurePath(p).name`: the last part, '' for none.
fn pathlib_name(p: &str) -> &str {
    p.split('/').rfind(|x| !x.is_empty() && *x != ".").unwrap_or("")
}

#[cfg(unix)]
fn bytes_path(b: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(b))
}

#[cfg(unix)]
fn path_bytes(p: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes()
}

/// `Path.home()`: `os.path.expanduser('~')`, HOME when set (trailing slashes stripped, '/' when nothing is left),
/// else the password database.
#[cfg(unix)]
fn home() -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let raw = match std::env::var_os("HOME") {
        Some(h) => h.as_bytes().to_vec(),
        None => {
            // SAFETY: getpwuid returns a pointer into static storage or null; the bytes are copied at once.
            // Python raises RuntimeError, which has no Exc kind; TypeError is likewise neither ValueError nor OSError.
            unsafe {
                let pw = libc::getpwuid(libc::getuid());
                if pw.is_null() || (*pw).pw_dir.is_null() {
                    return Err(Error::new(Exc::Type, "Could not determine home directory."));
                }
                std::ffi::CStr::from_ptr((*pw).pw_dir).to_bytes().to_vec()
            }
        }
    };
    let trimmed = raw.iter().rposition(|&c| c != b'/').map_or(&raw[..0], |i| &raw[..=i]);
    Ok(if trimmed.is_empty() { b"/".to_vec() } else { pure_path(trimmed) })
}

/// `_owner_auth()`: the login file a Codex child would otherwise share: $CODEX_HOME/auth.json, else
/// ~/.codex/auth.json.
#[cfg(unix)]
fn owner_auth() -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let base = match std::env::var_os("CODEX_HOME") {
        Some(v) if !v.is_empty() => v.as_bytes().to_vec(),
        _ => pure_join(&home()?, ".codex"),
    };
    Ok(bytes_path(pure_join(&base, "auth.json")))
}

fn os_err(e: &io::Error, path: &Path) -> Error {
    Error::os(py::os_error(e, Some(&path.to_string_lossy())))
}

/// `os.open(path, O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW, 0o600)` and the bytes written to it.
#[cfg(unix)]
fn create_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)
        .map_err(|e| os_err(&e, path))?;
    f.write_all(data).map_err(|e| Error::os(py::os_error(&e, None)))
}

#[cfg(unix)]
fn mkdir(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(mode).create(path).map_err(|e| os_err(&e, path))
}

/// `env.update(key=value)`: in place when the key is there, else appended.
fn env_set(env: &mut Env, key: &str, value: OsString) {
    match env.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = value,
        None => env.push((key.into(), value)),
    }
}

/// `_codex_home(root, env)`: give a Codex child a private CODEX_HOME holding only a copy of the owner's login
/// (and the owner's model catalog cache), and an empty HOME, both inside the call's temporary folder, owner-only.
/// Flags such as --ignore-user-config do not stop the CLI from loading the global $CODEX_HOME/AGENTS.md,
/// ~/.codex/skills, its memories or ~/.agents/skills; a fresh home does.
#[cfg(unix)]
fn codex_home(root: &Path, env: &mut Env) -> Result<Login> {
    let (home_dir, private) = (root.join("home"), root.join("codex"));
    mkdir(&home_dir, 0o700)?;
    mkdir(&private, 0o700)?;
    let owner = owner_auth()?;
    let snapshot = std::fs::read(&owner).ok(); // no login to copy: the call runs without one and fails the normal way
    let copy = private.join("auth.json");
    if let Some(bytes) = &snapshot {
        create_private(&copy, bytes)?;
    }
    // The owner's model catalog cache: without it every call's `codex debug models` (see lean_codex) fetches the
    // same catalog over the network, about 0.5-1 s; with it, about 15 ms. It holds no instructions. Not copied back.
    let cache = bytes_path(pure_join(&pure_parent(path_bytes(&owner)), "models_cache.json"));
    if let Ok(catalog) = std::fs::read(&cache) {
        create_private(&private.join("models_cache.json"), &catalog)?;
    }
    env_set(env, "HOME", home_dir.into_os_string());
    env_set(env, "CODEX_HOME", private.into_os_string());
    Ok(Login { owner, snapshot, copy })
}

/// `_lean_codex(argv, private, env, timeout_s)`: argv with Codex's agent prompt replaced by CODEX_INSTRUCTIONS and,
/// when the installed CLI can print its model catalog (`<argv[0]> debug models`, within timeout_s; Python's default
/// is 15), a copy without the tool modes, inserted before the trailing stdin marker. Both files live in the call's
/// private CODEX_HOME. A CLI without `debug models`, or a catalog in another format, keeps only the instructions
/// override. A catalog holding NaN, which Python writes back as NaN, also keeps only the instructions override.
pub fn lean_codex(argv: &[String], private: &Path, env: &Env, timeout_s: f64) -> Result<Vec<String>> {
    #[cfg(unix)]
    {
        let instructions = private.join("ringo-instructions.md");
        std::fs::write(&instructions, CODEX_INSTRUCTIONS).map_err(|e| os_err(&e, &instructions))?;
        let mut extra = vec!["-c".to_string(), format!("model_instructions_file={}", pyjson::quote(&instructions.to_string_lossy()))];
        let program = argv.first().ok_or_else(|| Error::value("list index out of range"))?;
        let listed = run_checked(&[program.clone(), "debug".into(), "models".into()], env, private, timeout_s);
        if let Some(catalog) = listed.map(|l| catalog_without_tools(&l)).transpose()?.flatten() {
            let path = private.join("ringo-catalog.json");
            if std::fs::write(&path, catalog).is_ok() {
                extra.extend(["-c".to_string(), format!("model_catalog_json={}", pyjson::quote(&path.to_string_lossy()))]);
            }
        }
        let at = if argv.last().is_some_and(|a| a == "-") { argv.len() - 1 } else { argv.len() };
        Ok([&argv[..at], &extra[..], &argv[at..]].concat())
    }
    #[cfg(not(unix))]
    {
        let _ = (argv, private, env, timeout_s);
        Err(Error::os(UNSUPPORTED))
    }
}

/// The catalog `debug models` printed, without the tool modes, as `json.dumps` writes it; None where Python's
/// handler swallows the error (ValueError, KeyError, TypeError, AttributeError); Err for RecursionError.
fn catalog_without_tools(listed: &[u8]) -> Result<Option<String>> {
    let constant = |name: &str| -> Result<Value> {
        match name {
            "NaN" => Err(Error::value("NaN cannot be kept")),
            "Infinity" => Ok(float_json(f64::INFINITY)),
            _ => Ok(float_json(f64::NEG_INFINITY)),
        }
    };
    let float = |text: &str| py_float(text).map(float_json);
    let decoder = Decoder { parse_constant: &constant, parse_float: &float, object_pairs_hook: &dict, max_depth: MAX_DEPTH };
    let mut catalog = match decoder.loads_bytes(listed) {
        Ok(Value::Object(m)) => m,
        Ok(_) => return Ok(None), // a list, str or number has no ['models']: TypeError
        Err(JsonFail::Recursion { what }) => return Err(recursion_error(what)),
        Err(_) => return Ok(None),
    };
    match catalog.get_mut("models") {
        Some(Value::Array(models)) => {
            for model in models {
                let Value::Object(m) = model else {
                    return Ok(None); // list.pop(field, None) is a TypeError, str.pop an AttributeError
                };
                for field in CATALOG_TOOL_FIELDS {
                    m.shift_remove(field);
                }
                m.insert("experimental_supported_tools".into(), Value::Array(Vec::new()));
            }
        }
        Some(Value::Object(m)) if m.is_empty() => {} // iterating a dict yields its keys: none here
        Some(Value::String(t)) if t.is_empty() => {} // and a str its characters
        _ => return Ok(None),
    }
    Ok(Some(pyjson::dumps(&Value::Object(catalog))))
}

/// `subprocess.run(argv, env=env, cwd=cwd, stdin=DEVNULL, capture_output=True, timeout=timeout_s,
/// check=True).stdout`, None where that raises OSError or a SubprocessError. Not a session of its own: on timeout
/// only the direct child is killed, as Python does.
#[cfg(unix)]
fn run_checked(argv: &[String], env: &Env, cwd: &Path, timeout_s: f64) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let deadline = Instant::now().checked_add(seconds(timeout_s));
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let (mut out, mut err) = (child.stdout.take()?, child.stderr.take()?);
    let (mut stdout, mut open, mut buf) = (Vec::new(), [true, true], vec![0u8; 32768]);
    let mut timed_out = false;
    while open[0] || open[1] {
        let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|r| r.is_zero()) {
            timed_out = true;
            break;
        }
        let ms = remaining.map_or(-1, |r| r.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32);
        let fd = |i: usize, raw: i32| libc::pollfd { fd: if open[i] { raw } else { -1 }, events: libc::POLLIN, revents: 0 };
        let mut fds = [fd(0, out.as_raw_fd()), fd(1, err.as_raw_fd())];
        // SAFETY: fds is a valid array of two pollfd for the call's duration.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        for (i, p) in fds.iter().enumerate() {
            if !open[i] || p.revents == 0 {
                continue;
            }
            let got = if i == 0 { out.read(&mut buf) } else { err.read(&mut buf) };
            match got {
                Ok(0) => open[i] = false,
                Ok(k) if i == 0 => stdout.extend_from_slice(&buf[..k]),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => open[i] = false,
            }
        }
    }
    let mut status = None;
    if !timed_out {
        let mut delay = 0.0005;
        loop {
            match child.try_wait() {
                Ok(Some(s)) => {
                    status = Some(s);
                    break;
                }
                Ok(None) => {}
                Err(_) => break,
            }
            let remaining = deadline.map_or(f64::INFINITY, |d| d.saturating_duration_since(Instant::now()).as_secs_f64());
            if remaining <= 0.0 {
                break;
            }
            delay = f64::min(delay * 2.0, remaining).min(0.05);
            std::thread::sleep(Duration::from_secs_f64(delay));
        }
    }
    match status {
        Some(s) => s.success().then_some(stdout),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// `_owner_lock(owner)`: serializes write-backs between Ringo processes, on <owner's folder>/.ringo-auth.lock
/// (created 0600, never through a symlink). The owner's own Codex does not take it, so a login it saves in the
/// instant between the final comparison and the rename can be overwritten; only Codex itself could close that window.
#[cfg(unix)]
struct OwnerLock(File);

#[cfg(unix)]
fn owner_lock(owner: &Path) -> std::result::Result<OwnerLock, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let path = bytes_path(pure_join(&pure_parent(path_bytes(owner)), ".ringo-auth.lock"));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&path)
        .map_err(|e| py::os_error(&e, Some(&path.to_string_lossy())))?;
    flock(f.as_raw_fd(), libc::LOCK_EX).map_err(|e| py::os_error(&e, None))?;
    Ok(OwnerLock(f))
}

#[cfg(unix)]
impl Drop for OwnerLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        let _ = flock(self.0.as_raw_fd(), libc::LOCK_UN);
    }
}

/// `_sync_codex_auth(owner, snapshot, copy)`: copy a login the child refreshed back to the owner, so the owner is
/// not left holding a rotated refresh token. Compare-and-swap: only when the copy changed, is JSON, and the owner's
/// file still equals the snapshot copied in; if the owner changed it meanwhile, theirs is kept. The write is atomic
/// (temp file in the same folder, fsync, rename), mode 0600, and never goes through a symlink. A failure is
/// reported on stderr, not raised; nesting too deep for json is raised, as in Python.
#[cfg(unix)]
fn sync_codex_auth(login: &Login) -> Result<()> {
    let Some(snapshot) = &login.snapshot else {
        return Ok(());
    };
    let Ok(new) = std::fs::read(&login.copy) else {
        return Ok(());
    };
    if &new == snapshot {
        return Ok(());
    }
    let accept = |_: &str| Ok(Value::Null);
    let float = |text: &str| py_float(text).map(float_json);
    let validator = Decoder { parse_constant: &accept, parse_float: &float, object_pairs_hook: &dict, max_depth: PY_MAX_DEPTH };
    match validator.loads_bytes(&new) {
        Ok(v) => dismantle(v),
        Err(JsonFail::Recursion { what }) => return Err(recursion_error(what)),
        Err(_) => return Ok(()), // half-written or garbage: never hand it to the owner
    }
    if let Err(reason) = write_back(&login.owner, snapshot, &new) {
        eprintln!(
            "ringo: Codex refreshed its login but it could not be saved to {} ({reason}); run `codex login` if later \
             calls are refused",
            login.owner.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn write_back(owner: &Path, snapshot: &[u8], new: &[u8]) -> std::result::Result<(), String> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let _lock = owner_lock(owner)?;
    if std::fs::symlink_metadata(owner).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{} is a symlink", owner.display()));
    }
    let current = std::fs::read(owner).map_err(|e| py::os_error(&e, Some(&owner.to_string_lossy())))?;
    if current != snapshot {
        return Ok(());
    }
    let parent = bytes_path(pure_parent(path_bytes(owner)));
    let (mut f, tmp) = mkstemp(".ringo-auth-", &parent).map_err(|e| e.msg)?; // created 0600
    let written = (|| -> io::Result<()> {
        f.write_all(new)?;
        f.flush()?;
        // SAFETY: fsync on the open file; os.fsync is fsync(2), not macOS's F_FULLFSYNC that File::sync_all uses.
        if unsafe { libc::fsync(f.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })()
    .map_err(|e| py::os_error(&e, None));
    drop(f);
    let replaced = written.and_then(|()| {
        std::fs::rename(&tmp, owner).map_err(|e| {
            format!("{}: {} -> {}", py::os_error(&e, None), py::repr(&tmp.to_string_lossy()), py::repr(&owner.to_string_lossy()))
        })
    });
    if replaced.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    replaced
}

// ---------- tempfile ----------

#[cfg(unix)]
const NAME_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";

/// tempfile's `_RandomNameSequence`: eight characters of [a-z0-9_].
#[cfg(unix)]
fn candidate_name() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
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

/// `os.path.normpath`.
#[cfg(unix)]
fn normpath(p: &[u8]) -> Vec<u8> {
    if p.is_empty() {
        return b".".to_vec();
    }
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
#[cfg(unix)]
fn abspath(p: &Path) -> PathBuf {
    let b = path_bytes(p);
    if b.starts_with(b"/") {
        return bytes_path(normpath(b));
    }
    let mut full = std::env::current_dir().map(|d| path_bytes(&d).to_vec()).unwrap_or_default();
    full.push(b'/');
    full.extend(b);
    bytes_path(normpath(&full))
}

/// `tempfile.gettempdir()`: the first of $TMPDIR, $TEMP, $TMP, /tmp, /var/tmp, /usr/tmp and the working folder in
/// which a file can be created, made absolute; found once per process.
#[cfg(unix)]
fn gettempdir() -> Result<PathBuf> {
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
        let dir = if dir.as_os_str() == "." { dir.clone() } else { abspath(dir) };
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
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => break,
            }
        }
    }
    let list = dirs.iter().map(|d| py::repr(&d.to_string_lossy())).collect::<Vec<_>>().join(", ");
    Err(Error::os(format!("[Errno 2] No usable temporary directory found in [{list}]")))
}

/// `tempfile.mkdtemp(prefix=prefix)`: a new folder, mode 0700, in gettempdir.
#[cfg(unix)]
fn mkdtemp(prefix: &str) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = gettempdir()?;
    for _ in 0..10_000 {
        let path = dir.join(format!("{prefix}{}", candidate_name()));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(os_err(&e, &path)),
        }
    }
    Err(Error::os("[Errno 17] No usable temporary directory name found"))
}

/// `tempfile.mkstemp(prefix=prefix, dir=dir)`: a new file, mode 0600, never through a symlink, and its absolute path.
#[cfg(unix)]
fn mkstemp(prefix: &str, dir: &Path) -> Result<(File, PathBuf)> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = abspath(dir);
    for _ in 0..10_000 {
        let path = dir.join(format!("{prefix}{}", candidate_name()));
        let opened =
            std::fs::OpenOptions::new().read(true).write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).mode(0o600).open(&path);
        match opened {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(os_err(&e, &path)),
        }
    }
    Err(Error::os("[Errno 17] No usable temporary file name found"))
}

/// `TemporaryDirectory` cleanup: everything under path removed, permissions and (macOS) file flags that block the
/// removal reset first, as tempfile's error handler does; a folder already gone is no error.
#[cfg(unix)]
fn rmtree(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
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
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(os_err(&e, path)),
    }
}

/// tempfile's `_resetperms` for a path that is not a symlink: no file flags, mode 0700. macOS changes the link
/// itself if the path became one meanwhile (lchflags, lchmod); elsewhere Python checks, then follows, and so does this.
#[cfg(unix)]
fn reset_perms(p: &Path) {
    let Ok(c) = std::ffi::CString::new(path_bytes(p)) else { return };
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

// ---------- the wait ----------

/// SIGINT during a native pipeline run (Python's KeyboardInterrupt): set by the handler `install_interrupt`
/// installs, polled by `wait`, which then kills the running call and fails with `keyboard_interrupt()`.
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Install the SIGINT handler for a native pipeline run (only then does `wait` poll for it). A second SIGINT
/// restores the default action and ends the process at once, as an impatient second Ctrl-C does.
pub fn install_interrupt() {
    #[cfg(unix)]
    {
        extern "C" fn on_sigint(_: libc::c_int) {
            if INTERRUPTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
                // SAFETY: signal and raise are async-signal-safe.
                unsafe {
                    libc::signal(libc::SIGINT, libc::SIG_DFL);
                    libc::raise(libc::SIGINT);
                }
            }
        }
        // SAFETY: the handler only touches an atomic and async-signal-safe calls; SA_RESTART keeps blocking
        // calls in other threads from failing with EINTR.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_sigint as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        }
        INSTALLED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// True once SIGINT arrived during a native pipeline run.
pub fn interrupted() -> bool {
    INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// `KeyboardInterrupt()`: `str()` is empty; `Error.data["type"]` names the class, which no `Exc` stands for
/// (it is not an Exception, so only a BaseException handler sees it).
pub fn keyboard_interrupt() -> Error {
    Error::new(Exc::Type, "").with_data(serde_json::json!({"type": "KeyboardInterrupt"}))
}

pub fn is_keyboard_interrupt(e: &Error) -> bool {
    e.data.as_ref().and_then(|d| d.get("type")).is_some_and(|t| t == "KeyboardInterrupt")
}

/// `snapshot()`: the first RAW_PEEK bytes of the running command's stdout and stderr, decoded with 'replace'.
pub type Snapshot = Arc<dyn Fn() -> Result<(String, String)> + Send + Sync>;

/// harness.py's `Watch` (and `Leash`), as run_isolated and `_wait` use it: one attempt's soft time limit.
/// With a watch, timeout_s is soft and the call is never killed at it. The calls, in order: run_isolated reads
/// `limit_s` before setup (the window, setup included) and `wait` reads it again; `wait` polls `stop_is_set` at least
/// every 0.25 s; once the call is `timeout_s` old, `soft` runs once, in a thread of its own, and the call goes on;
/// the call is stopped (Stopped) when `stop_is_set` turns true, or killed (TimeoutExpired, `limit`) at `limit_s`.
/// When the call is over, however it ended, run_isolated calls `set_done` and then joins the `soft` thread, so
/// `soft` must return soon after done is set.
pub trait Watch: Send + Sync {
    /// `watch.limit_s`: the hard limit in seconds, counted from the call's start, setup included.
    fn limit_s(&self) -> f64;
    /// `watch.stop.is_set()`: the person asked to stop the call.
    fn stop_is_set(&self) -> bool;
    /// `watch.done.set()`: the call is over.
    fn set_done(&self);
    /// `watch.soft(elapsed, snapshot)`: the call passed its soft limit `elapsed` seconds after it started;
    /// `snapshot()` reads what it has written so far.
    fn soft(&self, elapsed: f64, snapshot: Snapshot);
}

/// How `_wait` ended when the command did not exit: Python's 'limit' and 'stop'.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    Limit,
    Stop,
}

impl Ended {
    pub fn as_str(self) -> &'static str {
        match self {
            Ended::Limit => "limit",
            Ended::Stop => "stop",
        }
    }
}

/// Seconds as a Duration for a wait: negative waits not at all, NaN and overflow wait without end.
fn seconds(s: f64) -> Duration {
    if s <= 0.0 {
        return Duration::ZERO;
    }
    Duration::try_from_secs_f64(s).unwrap_or(Duration::MAX)
}

/// `_wait(proc, timeout_s, watch, snapshot, started, threads)`: how the wait ended: None (the command exited, not
/// reaped here), Limit or Stop. Times count from started, setup included. Without a watch, timeout_s is a hard limit.
/// With one it is soft: past it the command keeps running, `watch.soft(elapsed, snapshot)` runs once in a thread
/// (pushed to threads, for the caller to join), and the command is stopped when the watch says stop or at its limit.
pub fn wait(
    proc: &Child,
    timeout_s: f64,
    watch: Option<&Arc<dyn Watch>>,
    snapshot: &Snapshot,
    started: Instant,
    threads: &mut Vec<JoinHandle<()>>,
) -> Result<Option<Ended>> {
    #[cfg(unix)]
    {
        use std::sync::mpsc::RecvTimeoutError;
        let limit = watch.map_or(timeout_s, |w| w.limit_s());
        let mut fired = false;
        // A blocking wait in a thread wakes the moment the command exits (WNOWAIT: the caller still reaps it);
        // polling would add up to 50 ms to every call.
        let (tx, exited) = std::sync::mpsc::channel();
        let pid = proc.id();
        std::thread::Builder::new().spawn(move || {
            loop {
                // SAFETY: siginfo_t is plain data; waitid writes it and does not reap (WNOWAIT).
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) };
                if r == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
            let _ = tx.send(());
        })?;
        let polled = INSTALLED.load(std::sync::atomic::Ordering::SeqCst);
        loop {
            if polled && interrupted() {
                return Err(keyboard_interrupt());
            }
            let elapsed = started.elapsed().as_secs_f64();
            if elapsed >= limit {
                return Ok(Some(Ended::Limit));
            }
            if let Some(w) = watch {
                if w.stop_is_set() {
                    return Ok(Some(Ended::Stop));
                }
                if !fired && elapsed >= timeout_s {
                    fired = true;
                    let (w, snapshot) = (Arc::clone(w), Arc::clone(snapshot));
                    threads.push(std::thread::Builder::new().spawn(move || w.soft(elapsed, snapshot))?);
                }
            }
            let slice = if watch.is_none() && !polled { limit - elapsed } else { f64::min(0.25, limit - elapsed) };
            match exited.recv_timeout(seconds(slice)) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => return Ok(None),
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (proc, timeout_s, watch, snapshot, started, threads);
        Err(Error::os(UNSUPPORTED))
    }
}

/// `_spool(folder, name)`: a file a command writes to (or reads its stdin from) that peek can read while the command
/// runs: POSIX: an anonymous temporary file in gettempdir (folder and name are for Windows).
#[cfg(unix)]
fn spool(_folder: &Path, _name: &str) -> Result<File> {
    let (f, path) = mkstemp("tmp", &gettempdir()?)?;
    std::fs::remove_file(&path).map_err(|e| os_err(&e, &path))?;
    Ok(f)
}

/// `_peek(f)`: the first RAW_PEEK bytes of a spool file, without moving the position the command writes at.
#[cfg(unix)]
fn peek(f: &File) -> Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut buf = vec![0u8; RAW_PEEK];
    let n = loop {
        match f.read_at(&mut buf, 0) {
            Ok(n) => break n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    };
    buf.truncate(n);
    Ok(buf)
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

// ---------- run_isolated ----------

/// run_isolated's keywords (all off by default, as in Python).
#[derive(Clone, Default)]
pub struct Isolated {
    /// Remove every *API_KEY variable and the endpoint and token overrides: the CLI's own subscription login is used.
    pub subscription_auth: bool,
    /// The command is a Codex CLI: it gets a private CODEX_HOME and HOME (see codex_home).
    pub codex_home: bool,
    /// With codex_home, a single tool-free Codex turn: its agent prompt and tool modes are replaced (see lean_codex).
    pub lean: bool,
    /// timeout_s becomes a soft limit (see Watch and wait); the call is never killed at it.
    pub watch: Option<Arc<dyn Watch>>,
}

/// The variables a subscription call never passes on.
#[cfg(unix)]
fn scrubbed(key: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let k = key.as_bytes();
    k.ends_with(b"API_KEY")
        || [
            "OPENAI_BASE_URL",
            "OPENAI_API_BASE",
            "OPENAI_ACCESS_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
            "OPENAI_IDENTITY_TOKEN_FILE",
            "OPENAI_WORKLOAD_IDENTITY_PROVIDER_ID",
        ]
        .iter()
        .any(|n| k == n.as_bytes())
}

#[cfg(unix)]
struct Call {
    argv: Vec<String>,
    started: Instant,
    ended: Option<Ended>,
    returncode: Option<i32>,
    stdout: String,
    stderr: String,
}

/// `run_isolated(argv, prompt, timeout_s, *, subscription_auth, codex_home, lean, watch)`: stdout of one run. The
/// working folder is empty, so no project instructions or files load from it; it is not a file-system boundary.
/// The command gets the caller's environment without the TypeSafe key, the prompt on stdin, and a process tree of
/// its own (see spawn), so a timeout kills it and everything it started. With codex_home a refreshed login is
/// written back even when the call fails or times out. A failure (JudgeError: "<name> exited with <code>: <stderr
/// tail>"), a stop (Stopped) or a timeout (TimeoutExpired, `limit` true) carries the stdout written so far in
/// `data["stdout"]`.
pub fn run_isolated(argv: &[String], prompt: &str, timeout_s: f64, kw: Isolated) -> Result<String> {
    #[cfg(unix)]
    {
        // ceiling: stdout is read whole; a command that floods it fills disk and memory
        let mut env: Env = std::env::vars_os().filter(|(k, _)| k != "TYPESAFE_API_KEY").collect();
        if kw.subscription_auth {
            env.retain(|(k, _)| !scrubbed(k));
        }
        let tmp = mkdtemp("council-judge-")?;
        let inside = isolated_call(&tmp, argv, prompt, timeout_s, &kw, env);
        let cleanup = rmtree(&tmp);
        let call = match inside {
            Ok(call) => call,
            Err(e) => {
                cleanup?;
                return Err(e);
            }
        };
        cleanup?;
        if let Some(ended) = call.ended {
            let timeout = round1(call.started.elapsed().as_secs_f64());
            let exc = if ended == Ended::Stop { stopped(&call.argv, timeout) } else { timeout_expired(&call.argv, timeout) };
            // the events before the kill: what the call cost, and why it hung
            return Err(with_fields(exc, serde_json::json!({"limit": ended == Ended::Limit, "stdout": call.stdout})));
        }
        let code = call.returncode.unwrap_or(0); // a launch happened: without one, ended is Limit
        if code != 0 {
            let squeezed = py::split(&call.stderr).join(" ");
            let tail = py::tail(&squeezed, 300);
            let mut message = format!("{} exited with {code}", pathlib_name(&call.argv[0]));
            if !tail.is_empty() {
                message.push_str(": ");
                message.push_str(tail);
            }
            // a CLI can report on stdout what the failed call cost
            return Err(with_fields(judge_error(message, false), serde_json::json!({"stdout": call.stdout})));
        }
        Ok(call.stdout)
    }
    #[cfg(not(unix))]
    {
        let _ = (argv, prompt, timeout_s, kw);
        Err(Error::os(UNSUPPORTED))
    }
}

/// The inside of run_isolated's `with` block: spools, the call, the finally (done, joins, login write-back), and the
/// output read back.
#[cfg(unix)]
fn isolated_call(tmp: &Path, argv: &[String], prompt: &str, timeout_s: f64, kw: &Isolated, mut env: Env) -> Result<Call> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::process::ExitStatusExt;
    // Output goes to files, not pipes, so the wait ends when the command exits, even if something it started still
    // holds stdout open; whatever it started is then killed with its session. stdin is a file too.
    let out = Arc::new(spool(tmp, "out")?);
    let err = Arc::new(spool(tmp, "err")?);
    let mut stdin = spool(tmp, "stdin")?;
    stdin.write_all(prompt.as_bytes())?;
    stdin.seek(SeekFrom::Start(0))?;
    let cwd = tmp.join("work");
    std::fs::create_dir(&cwd).map_err(|e| os_err(&e, &cwd))?;
    let started = Instant::now(); // the limit counts setup too
    let (mut ended, mut returncode, mut threads) = (None, None, Vec::new());
    let limit = kw.watch.as_ref().map_or(timeout_s, |w| w.limit_s());
    let login = if kw.codex_home { Some(codex_home(tmp, &mut env)?) } else { None };
    let snapshot: Snapshot = {
        let (out, err) = (Arc::clone(&out), Arc::clone(&err));
        Arc::new(move || Ok((lossy(&peek(&out)?), lossy(&peek(&err)?))))
    };
    let mut argv = argv.to_vec();
    let body = (|| -> Result<()> {
        if kw.codex_home && kw.lean {
            // max(0.1, min(15, limit)): Python's min keeps 15 when limit is NaN, where clamp would give NaN
            let within = if limit.is_nan() { 15.0 } else { limit.clamp(0.1, 15.0) };
            argv = lean_codex(&argv, &tmp.join("codex"), &env, within)?;
        }
        if started.elapsed().as_secs_f64() >= limit {
            ended = Some(Ended::Limit); // setup used the whole window: the model is never launched
            return Ok(());
        }
        let mut proc = spawn(
            &argv,
            Popen {
                stdin: Some(stdin.try_clone()?.into()),
                stdout: Some(out.try_clone()?.into()),
                stderr: Some(err.try_clone()?.into()),
                cwd: Some(cwd.clone()),
                env: Some(env.clone()),
            },
        )?;
        let waited = wait(&proc, timeout_s, kw.watch.as_ref(), &snapshot, started, &mut threads);
        if let Err(e) = kill_tree(&proc)
            && !matches!(e.raw_os_error(), Some(libc::ESRCH | libc::EPERM))
        {
            return Err(e.into());
        }
        let status = proc.wait()?;
        returncode = Some(status.code().unwrap_or_else(|| -status.signal().unwrap_or(0)));
        ended = waited?;
        Ok(())
    })();
    if let Some(w) = &kw.watch {
        w.set_done();
    }
    for thread in threads {
        let _ = thread.join(); // its probe stops on done, and is recorded, before this call returns
    }
    if let Some(login) = &login {
        sync_codex_auth(login)?;
    }
    body?;
    let read = |f: &File| -> Result<String> {
        let mut data = Vec::new();
        let mut f = f;
        f.seek(SeekFrom::Start(0))?;
        f.read_to_end(&mut data)?;
        Ok(lossy(&data))
    };
    let (stdout, stderr) = (read(&out)?, read(&err)?);
    Ok(Call { argv, started, ended, returncode, stdout, stderr })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round1_is_pythons_round() {
        for (x, want) in [(0.25, 0.2), (0.35, 0.3), (0.05, 0.1), (1.15, 1.1), (2.5, 2.5), (12.34, 12.3), (0.0, 0.0), (9.96, 10.0)] {
            assert_eq!(round1(x), want, "{x}");
        }
    }

    #[test]
    fn pure_paths_match_pathlib() {
        for (p, want) in [("", "."), (".", "."), ("/", "/"), ("//", "//"), ("///a", "/a"), ("//a/./b/", "//a/b"), ("a/../b", "a/../b")] {
            assert_eq!(pure_path(p.as_bytes()), want.as_bytes(), "{p}");
        }
        assert_eq!(pure_join(b".", "auth.json"), b"auth.json");
        assert_eq!(pure_parent(b"auth.json"), b".");
        assert_eq!(pure_parent(b"/auth.json"), b"/");
        assert_eq!(pure_parent(b"//auth.json"), b"//");
        assert_eq!(pathlib_name("/usr/bin/"), "bin");
        assert_eq!(pathlib_name(".."), "..");
        assert_eq!(pathlib_name("a/."), "a");
        assert_eq!(pathlib_name("/"), "");
    }

    #[test]
    fn infinity_survives_as_python_writes_it() {
        assert_eq!(pyjson::dumps(&float_json(f64::INFINITY)), "Infinity");
        assert_eq!(pyjson::dumps(&float_json(f64::NEG_INFINITY)), "-Infinity");
        assert_eq!(pyjson::dumps(&float_json(1.5)), "1.5");
    }
}
