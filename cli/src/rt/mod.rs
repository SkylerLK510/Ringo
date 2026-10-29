//! Ringo's pipeline runtime in Rust: review, design and work, ported from the Python modules of the same names
//! (`context.py`, `pipelines.py`, `findings.py`, `backends.py`, `harness.py`, `workflow.py`). Each module mirrors
//! its Python original's functions and messages, so every model call receives the same bytes; the differential
//! gate (`tests/gate.rs` with `tests/gate/`) checks that against the Python modules with a replaying backend.

pub mod backends;
pub mod context;
pub mod findings;
pub mod harness;
pub mod pipelines;
pub mod py;
pub mod pyjson;
pub mod wait;
pub mod workflow;

use std::fmt;

/// The Python exception class an error stands for. `name()` is what Python prints as `type(exc).__name__`, which
/// some records include (state.json's error is "<name>: <message>").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exc {
    Value,
    Os,
    Key,
    Type,
    Context,
    Judge,
    Call,
    Stopped,
    Timeout,
    Budget,
    Admission,
    NeedVerdicts,
}

impl Exc {
    pub fn name(self) -> &'static str {
        match self {
            Exc::Value => "ValueError",
            Exc::Os => "OSError",
            Exc::Key => "KeyError",
            Exc::Type => "TypeError",
            Exc::Context => "ContextError",
            Exc::Judge => "JudgeError",
            Exc::Call => "CallError",
            Exc::Stopped => "Stopped",
            Exc::Timeout => "TimeoutExpired",
            Exc::Budget => "BudgetExceeded",
            Exc::Admission => "AdmissionRefused",
            Exc::NeedVerdicts => "NeedVerdicts",
        }
    }

    /// Python's `except X` also catches subclasses: ContextError is a ValueError, CallError a JudgeError,
    /// AdmissionRefused a BudgetExceeded, Stopped a TimeoutExpired.
    pub fn is(self, base: Exc) -> bool {
        self == base
            || matches!(
                (self, base),
                (Exc::Context, Exc::Value) | (Exc::Call, Exc::Judge) | (Exc::Admission, Exc::Budget) | (Exc::Stopped, Exc::Timeout)
            )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Error {
    pub exc: Exc,
    pub msg: String,
    /// Structured payload some exceptions carry in Python (e.g. CallError's attempt record), as JSON.
    pub data: Option<serde_json::Value>,
}

impl Error {
    pub fn new(exc: Exc, msg: impl Into<String>) -> Error {
        Error { exc, msg: msg.into(), data: None }
    }
    pub fn value(msg: impl Into<String>) -> Error {
        Error::new(Exc::Value, msg)
    }
    pub fn os(msg: impl Into<String>) -> Error {
        Error::new(Exc::Os, msg)
    }
    pub fn with_data(mut self, data: serde_json::Value) -> Error {
        self.data = Some(data);
        self
    }
    pub fn is(&self, base: Exc) -> bool {
        self.exc.is(base)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::os(py::os_error(&e, None))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// How `ringo review|design|work` runs, decided before any model call (the release gate).
#[derive(Debug)]
pub enum Selection {
    /// This binary runs the pipeline; the value is the implementation identity its records carry.
    Native(serde_json::Value),
    /// Python's `ringo.py` must run it: an active release decides which code runs, and verifying and approving a
    /// release stays in one place (releases.py), which then runs either its Python modules or its bundled binary.
    Python,
}

/// The release gate for the native runtime. RINGO_PINNED (set by releases.py when it runs a native release's
/// bundled binary) is honoured only when this program is that bundle's binary. Without it, any release state
/// that exists hands the run to Python; with none, this binary runs as checkout code, as ringo.py would run its
/// own checkout. On platforms other than macOS and Linux the Python runtime always runs.
pub fn select(home: &std::path::Path) -> Result<Selection> {
    if !cfg!(unix) {
        return Ok(Selection::Python);
    }
    let exe = std::env::current_exe().and_then(|p| p.canonicalize())?;
    let binary_sha256 = sha256_hex(&std::fs::read(&exe)?);
    if let Some(pinned) = std::env::var_os("RINGO_PINNED") {
        let mut meta = pyjson::loads(&pinned.to_string_lossy())?;
        let root = meta.get("root").and_then(|r| r.as_str()).ok_or_else(|| Error::value("RINGO_PINNED has no release root"))?;
        let want = std::path::Path::new(root).join("bin/ringo").canonicalize().ok();
        if want.as_deref() != Some(exe.as_path()) {
            return Err(Error::value("RINGO_PINNED names a release whose binary is not this program"));
        }
        let map = meta.as_object_mut().ok_or_else(|| Error::value("RINGO_PINNED is not an object"))?;
        map.insert("binary_sha256".into(), binary_sha256.into());
        return Ok(Selection::Native(meta));
    }
    // releases.state_root(): the same refusals, so a bad RINGO_RELEASE_HOME fails here as it fails in Python.
    let root = crate::state_root();
    if root.is_symlink() {
        return Err(Error::value("release state cannot be a symlink"));
    }
    let resolved = resolve(&root);
    let here = resolve(home);
    if resolved == here || resolved.starts_with(&here) {
        return Err(Error::value("release state must be outside the source repository"));
    }
    let active = resolved.join("active.json");
    if active.exists() || active.is_symlink() {
        return Ok(Selection::Python);
    }
    let mut files = serde_json::Map::new();
    for (dir, ext) in [("prompts", "md"), ("plans", "json")] {
        let mut names: Vec<_> = std::fs::read_dir(home.join(dir))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == ext) && p.is_file())
            .collect();
        names.sort();
        for path in names {
            let name = format!("{dir}/{}", path.file_name().unwrap().to_string_lossy());
            files.insert(name, sha256_hex(&std::fs::read(&path)?).into());
        }
    }
    let code = serde_json::json!({"binary": binary_sha256, "files": files});
    Ok(Selection::Native(serde_json::json!({
        "mode": "checkout",
        "id": format!("checkout:native:{}", sha256_hex(canonical(&code).as_bytes())),
        "root": here.to_string_lossy(),
        "quality_claim": null,
        "runtime": "native",
        "binary_sha256": binary_sha256,
    })))
}

/// latency.canonical(): sorted keys, no spaces, ASCII only.
pub fn canonical(value: &serde_json::Value) -> String {
    pyjson::dumps_with(value, pyjson::Opts { separators: Some((",", ":")), sort_keys: true, ..Default::default() })
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// pathlib's resolve(strict=False): the longest existing prefix canonicalised, the rest appended.
pub fn resolve(path: &std::path::Path) -> std::path::PathBuf {
    let absolute = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(path) };
    let mut rest = Vec::new();
    let mut probe = absolute.as_path();
    loop {
        if let Ok(real) = probe.canonicalize() {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (probe.parent(), probe.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                probe = parent;
            }
            _ => return absolute,
        }
    }
}
