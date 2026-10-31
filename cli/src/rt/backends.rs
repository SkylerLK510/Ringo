//! Ported from `backends.py`: backend command presets, isolated subprocess execution and strict reply-object
//! parsing. A model call runs in an empty working folder, with the caller's environment minus the TypeSafe key (and,
//! for subscription calls, minus API keys and endpoint overrides), in a process session of its own that a timeout
//! or a stop kills whole. A Codex child also gets a private CODEX_HOME and HOME holding only a copy of the owner's
//! login (and model cache); a login it refreshed is written back to the owner under a lock, compare-and-swap.
//!
//! JSON is read with `pyjson::Decoder`, CPython 3.14's `json` C scanner (positions, messages, hooks), because
//! `top_level_objects` charges failed attempts by the error positions it reports.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use crate::rt::pyjson::{self, Decoder, JsonFail, skip_ws};
use crate::rt::{Error, Exc, Result, py};
use serde_json::{Map, Value};
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
pub const JUDGE_PRESETS: [(&str, &str); 4] = [
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
         -c include_collaboration_mode_instructions=false -c analytics.enabled=false \
         -c 'project_doc_max_bytes=0' -c 'forced_login_method=\"chatgpt\"' \
         -c 'model_provider=\"openai\"' -c 'web_search=\"disabled\"' \
         --skip-git-repo-check --ephemeral -s read-only --color never --json -",
    ),
    (
        "codex-search",
        // research's gather call (research.py): the codex-chatgpt restrictions with live web search on
        "codex --search exec --ignore-user-config --ignore-rules --disable apps --disable plugins \
         --disable hooks --disable shell_tool --disable unified_exec --disable skill_search \
         --enable skip_host_skill_discovery --disable unbounded_connection_retries \
         --disable sleep_tool --disable goals --disable view_image --disable image_generation \
         --disable default_mode_request_user_input -c include_environment_context=false \
         -c include_permissions_instructions=false -c skills.include_instructions=false \
         -c include_collaboration_mode_instructions=false -c analytics.enabled=false \
         -c 'project_doc_max_bytes=0' -c 'forced_login_method=\"chatgpt\"' \
         -c 'model_provider=\"openai\"' -c 'web_search=\"live\"' \
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
        .with_data(serde_json::json!({"cmd": cmd, "timeout": pyjson::float_value(timeout)}))
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
        Err(io::Error::other(crate::rt::NATIVE_ONLY))
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
                Error::io(&io::Error::from_raw_os_error(code - CHDIR_FAILED), shown.as_deref())
            }
            _ => Error::io(&e, Some(program)),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (argv, kw);
        Err(Error::os(crate::rt::NATIVE_ONLY))
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
        Err(io::Error::other(crate::rt::NATIVE_ONLY))
    }
}

// ---------- strict JSON ----------

/// `_no_constant(name)`: a parse_constant hook that refuses NaN, Infinity and -Infinity.
pub fn no_constant(name: &str) -> Result<f64> {
    Err(Error::value(format!("{name} is not a JSON number")))
}

/// `_finite_float(text)`: a parse_float hook that refuses a number that overflows to infinity.
pub fn finite_float(text: &str) -> Result<f64> {
    let value = py::float(text)?;
    if !value.is_finite() {
        return Err(Error::value(format!("{text} is not a finite number")));
    }
    Ok(value)
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
        Ok(pyjson::float_value(0.0))
    };
    let number = |token: &str| -> Result<Value> {
        let value = py::float(token)?;
        if !value.is_finite() {
            refused.set(true);
        }
        Ok(pyjson::float_value(value))
    };
    let pairs = |pairs: Vec<(String, Value)>| -> Result<Value> {
        let obj = object(pairs);
        repeats.set(obj.repeats_a_key);
        Ok(Value::Object(obj.map))
    };
    let decoder = Decoder { parse_constant: &constant, parse_float: &number, object_pairs_hook: &pairs, ..Decoder::default() };
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
impl Decoder<'static> {
    /// `json.loads(text, parse_constant=_no_constant, parse_float=_finite_float)`: plain dicts, no NaN or
    /// infinities.
    pub fn strict() -> Decoder<'static> {
        Decoder { parse_constant: &strict_constant, parse_float: &strict_float, ..Decoder::default() }
    }
}

fn strict_constant(name: &str) -> Result<Value> {
    no_constant(name).map(pyjson::float_value)
}

fn strict_float(text: &str) -> Result<Value> {
    finite_float(text).map(pyjson::float_value)
}

// ---------- the private Codex home ----------

/// The login file the owner's Codex uses and where it lives: `_codex_home`'s (owner, snapshot, copy).
struct Login {
    owner: PathBuf,
    snapshot: Option<Vec<u8>>,
    copy: PathBuf,
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
            unsafe {
                let pw = libc::getpwuid(libc::getuid());
                if pw.is_null() || (*pw).pw_dir.is_null() {
                    return Err(Error::new(Exc::Runtime, "Could not determine home directory."));
                }
                std::ffi::CStr::from_ptr((*pw).pw_dir).to_bytes().to_vec()
            }
        }
    };
    let trimmed = raw.iter().rposition(|&c| c != b'/').map_or(&raw[..0], |i| &raw[..=i]);
    Ok(if trimmed.is_empty() { b"/".to_vec() } else { py::pure_path(trimmed) })
}

/// `_owner_auth()`: the login file a Codex child would otherwise share: $CODEX_HOME/auth.json, else
/// ~/.codex/auth.json.
#[cfg(unix)]
fn owner_auth() -> Result<PathBuf> {
    let base = match std::env::var_os("CODEX_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => py::join(&py::path_of(home()?), ".codex"),
    };
    Ok(py::join(&base, "auth.json"))
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
        .map_err(|e| py::path_error(&e, path))?;
    f.write_all(data).map_err(Error::from)
}

/// `env.update(key=value)`: in place when the key is there, else appended.
pub(crate) fn env_set(env: &mut Env, key: &str, value: OsString) {
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
    py::mkdir(&home_dir, 0o700, false, false)?;
    py::mkdir(&private, 0o700, false, false)?;
    let owner = owner_auth()?;
    let snapshot = std::fs::read(&owner).ok(); // no login to copy: the call runs without one and fails the normal way
    let copy = private.join("auth.json");
    if let Some(bytes) = &snapshot {
        create_private(&copy, bytes)?;
    }
    // The owner's model catalog cache: without it every call's `codex debug models` (see lean_codex) fetches the
    // same catalog over the network, about 0.5-1 s; with it, about 15 ms. It holds no instructions. Not copied back.
    let cache = py::join(&py::parent(&owner), "models_cache.json");
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
        std::fs::write(&instructions, CODEX_INSTRUCTIONS).map_err(|e| py::path_error(&e, &instructions))?;
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
        Err(Error::os(crate::rt::NATIVE_ONLY))
    }
}

/// The catalog `debug models` printed, without the tool modes, as `json.dumps` writes it; None where Python's
/// handler swallows the error (ValueError, KeyError, TypeError, AttributeError); Err for RecursionError.
fn catalog_without_tools(listed: &[u8]) -> Result<Option<String>> {
    let constant = |name: &str| -> Result<Value> {
        match name {
            "NaN" => Err(Error::value("NaN cannot be kept")),
            "Infinity" => Ok(pyjson::float_value(f64::INFINITY)),
            _ => Ok(pyjson::float_value(f64::NEG_INFINITY)),
        }
    };
    let float = |text: &str| py::float(text).map(pyjson::float_value);
    let decoder = Decoder { parse_constant: &constant, parse_float: &float, ..Decoder::default() };
    let mut catalog = match decoder.loads_bytes(listed) {
        Ok(Value::Object(m)) => m,
        Ok(_) => return Ok(None), // a list, str or number has no ['models']: TypeError
        Err(f @ JsonFail::Recursion { .. }) => return Err(f.into_error("")),
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

/// `debug models` as `_lean_codex` runs it: stdout, or None on failure, timeout or Ctrl-C. In a process group of its
/// own, killed whole when it does not finish, as Python's spawn/kill_tree do for it.
#[cfg(unix)]
fn run_checked(argv: &[String], env: &Env, cwd: &Path, timeout_s: f64) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let deadline = Instant::now().checked_add(seconds(timeout_s));
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .process_group(0)
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
        if remaining.is_some_and(|r| r.is_zero()) || interrupted() {
            timed_out = true;
            break;
        }
        // at most 250 ms per wait, so Ctrl-C is noticed as quickly as during the call itself
        let ms = remaining.map_or(250, |r| r.as_micros().div_ceil(1000).min(250) as i32);
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
            if remaining <= 0.0 || interrupted() {
                break;
            }
            delay = f64::min(delay * 2.0, remaining).min(0.05);
            std::thread::sleep(Duration::from_secs_f64(delay));
        }
    }
    // SAFETY: the group is this child's own (process_group(0)); killing it ends anything it started.
    unsafe {
        libc::killpg(child.id() as i32, libc::SIGKILL);
    }
    match status {
        Some(s) if !interrupted() => s.success().then_some(stdout),
        _ => {
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
    let path = py::join(&py::parent(owner), ".ringo-auth.lock");
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
    // Only a regular file the child left in its copy's place: never through a symlink (that would hand the owner
    // another file), and never a FIFO or device (opened non-blocking, so one cannot hang the write-back).
    let Ok(new) = read_no_follow(&login.copy) else {
        return Ok(());
    };
    if &new == snapshot {
        return Ok(());
    }
    let accept = |_: &str| Ok(Value::Null);
    let float = |text: &str| py::float(text).map(pyjson::float_value);
    let validator = Decoder { parse_constant: &accept, parse_float: &float, max_depth: PY_MAX_DEPTH, ..Decoder::default() };
    match validator.loads_bytes(&new) {
        Ok(v) => pyjson::dispose(v),
        Err(f @ JsonFail::Recursion { .. }) => return Err(f.into_error("")),
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

fn read_no_follow(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(data)
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
    let parent = py::parent(owner);
    let (mut f, tmp) = py::mkstemp(".ringo-auth-", &parent).map_err(|e| e.msg)?; // created 0600
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
    let replaced = written.and_then(|()| py::replace(&tmp, owner).map_err(|e| e.msg));
    if replaced.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    replaced
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

/// `KeyboardInterrupt()`: `str()` is empty.
pub fn keyboard_interrupt() -> Error {
    Error::new(Exc::KeyboardInterrupt, "")
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
        Err(Error::os(crate::rt::NATIVE_ONLY))
    }
}

/// `_spool(folder, name)`: a file a command writes to (or reads its stdin from) that peek can read while the command
/// runs: POSIX: an anonymous temporary file in gettempdir (folder and name are for Windows).
#[cfg(unix)]
fn spool(_folder: &Path, _name: &str) -> Result<File> {
    let (f, path) = py::mkstemp("tmp", &py::gettempdir()?)?;
    std::fs::remove_file(&path).map_err(|e| py::path_error(&e, &path))?;
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
            // Claude's endpoint override and its cloud-provider switches would also move a call off the login
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
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
        let tmp = py::mkdtemp("council-judge-")?;
        let inside = isolated_call(&tmp, argv, prompt, timeout_s, &kw, env);
        let cleanup = py::rmtree(&tmp);
        let call = match inside {
            Ok(call) => call,
            Err(e) => {
                cleanup?;
                return Err(e);
            }
        };
        cleanup?;
        if let Some(ended) = call.ended {
            let timeout = py::round(call.started.elapsed().as_secs_f64(), 1);
            let exc = if ended == Ended::Stop { stopped(&call.argv, timeout) } else { timeout_expired(&call.argv, timeout) };
            // the events before the kill: what the call cost, and why it hung
            return Err(with_fields(exc, serde_json::json!({"limit": ended == Ended::Limit, "stdout": call.stdout})));
        }
        let code = call.returncode.unwrap_or(0); // a launch happened: without one, ended is Limit
        if code != 0 {
            let squeezed = py::split(&call.stderr).join(" ");
            let tail = py::tail(&squeezed, 300);
            let mut message = format!("{} exited with {code}", py::path_name(&call.argv[0]));
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
        Err(Error::os(crate::rt::NATIVE_ONLY))
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
    std::fs::create_dir(&cwd).map_err(|e| py::path_error(&e, &cwd))?;
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
            if interrupted() {
                return Err(keyboard_interrupt()); // Ctrl-C during setup: the model is never launched
            }
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

