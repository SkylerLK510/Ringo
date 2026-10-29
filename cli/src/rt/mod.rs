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
