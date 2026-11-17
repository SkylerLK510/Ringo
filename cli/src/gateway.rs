//! Read-only gateway over tasks and registered evidence, independent of any transport.
//!
//! A host (an OpenCode tool bridge, a shell command, a test) builds one `Gateway` for its repository and feeds it
//! request envelopes; responses are self-describing JSON. The gateway performs reads only: no model calls, no
//! mutations, no nested work. Evidence is reached exclusively through registered immutable snapshots by opaque ID —
//! a path is never an access authority — and every response reports protocol version, request ID, task revision and
//! structured errors.

use crate::evidence::{artifact_origin, valid_digest, EvidenceStore};
use crate::task::{RepositoryBinding, Task, TaskStore};
use crate::task_assignment::AssignmentStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub const GATEWAY_PROTOCOL: u32 = 1;
/// Host policy, not a request parameter: a batch runs at most this many independent read steps.
pub const MAX_BATCH_STEPS: usize = 8;
/// Host policy: at most this many dispatches run at once per gateway instance. A host that wants process-wide
/// admission shares one gateway instance. Excess requests are refused immediately; there is no queue.
pub const MAX_CONCURRENCY: usize = 2;
/// Host policy: the aggregate serialized response envelope, including every step of a batch, stays under this.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024;
/// Host policy: one read step returns at most this many bytes of snapshot content.
pub const MAX_READ_BYTES: usize = 16 * 1024;
/// Host policy: an oversized request is refused without execution.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
/// Host policy: a request (a whole batch) must finish its reads inside this deadline; later steps are skipped,
/// never half-executed, and earlier results are returned.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(10);
/// Room held back from content so the terminal status (ok, complete, per-step statuses) always fits.
const TERMINAL_STATUS_RESERVE: usize = 1024;
/// A batch step is only started when at least this much room is left for its own envelope.
const MIN_STEP_ROOM: usize = 256;
/// Error details are diagnostics, not required state; they are bounded.
const MAX_DETAIL_BYTES: usize = 512;

/// Everything the caller did wrong or that the store could not honor, as a stable code plus bounded detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayError {
    pub code: &'static str,
    pub detail: String,
}

impl GatewayError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self { code, detail: bounded_detail(detail.into()) }
    }
    fn invalid(detail: impl Into<String>) -> Self {
        Self::new("invalid_request", detail)
    }
    fn to_json(&self) -> Value {
        json!({ "code": self.code, "detail": self.detail })
    }
}

/// Detail is diagnostics; it is clamped on a character boundary so it can never dominate a response.
fn bounded_detail(mut detail: String) -> String {
    if detail.len() <= MAX_DETAIL_BYTES {
        return detail;
    }
    let mut cut = MAX_DETAIL_BYTES;
    while !detail.is_char_boundary(cut) {
        cut -= 1;
    }
    detail.truncate(cut);
    detail.push('…');
    detail
}

/// One admitted execution slot. Held for the whole dispatch; freed on drop, including on panic.
pub struct AdmissionGuard<'a>(&'a AtomicUsize);

impl Drop for AdmissionGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct Gateway {
    tasks: TaskStore,
    evidence: EvidenceStore,
    assignments: AssignmentStore,
    in_flight: AtomicUsize,
}

impl Gateway {
    pub fn new(state_root: &Path, repository: &RepositoryBinding) -> Result<Self, String> {
        let tasks = TaskStore::new(state_root, repository.path.clone(), repository.git)
            .map_err(|e| e.to_string())?;
        let evidence = EvidenceStore::new(state_root, repository).map_err(|e| e.to_string())?;
        let assignments = AssignmentStore::new(state_root, repository).map_err(|e| e.to_string())?;
        Ok(Self { tasks, evidence, assignments, in_flight: AtomicUsize::new(0) })
    }

    /// Admit one dispatch, or `None` when the concurrency limit is already met: callers are refused, not queued.
    pub fn try_admit(&self) -> Option<AdmissionGuard<'_>> {
        let held = self.in_flight.fetch_add(1, Ordering::Relaxed);
        if held >= MAX_CONCURRENCY {
            self.in_flight.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(AdmissionGuard(&self.in_flight))
    }

    /// Entry point for transports that already parse JSON. Never fails: every problem is a structured response.
    pub fn dispatch(&self, request: &Value) -> Value {
        self.dispatch_at(request, Instant::now())
    }

    /// The dispatch itself with the request clock supplied, so a test can exercise the deadline paths without
    /// sleeping. A host always uses `dispatch`.
    fn dispatch_at(&self, request: &Value, start: Instant) -> Value {
        let _guard = match self.try_admit() {
            Some(guard) => guard,
            None => return self.envelope(None, None, None, Err(GatewayError::new("busy",
                "another request is running; the concurrency limit is met and excess requests are refused, not queued"))),
        };
        let request_id = request.get("request_id").and_then(Value::as_str).map(str::to_owned);
        let op = request.get("op").and_then(Value::as_str).map(str::to_owned);
        // Only bounded input is ever echoed. A refusal for an oversized request must not repeat an unbounded field,
        // or the refusal itself would break the response bound it is protecting.
        let echoed_id = request_id.as_deref().filter(|id| !id.is_empty() && id.len() <= 200);
        let echoed_op = op.as_deref().filter(|op| op.len() <= 64);
        // The whole request must fit the input bound, whatever transport handed it over.
        let size = serde_json::to_vec(request).map(|bytes| bytes.len()).unwrap_or(usize::MAX);
        if size > MAX_REQUEST_BYTES {
            return self.envelope(echoed_id, echoed_op, None,
                                 Err(GatewayError::new("request_too_large",
                                     format!("the request is {size} bytes; the limit is {MAX_REQUEST_BYTES}"))));
        }
        if request.get("protocol").and_then(Value::as_u64) != Some(GATEWAY_PROTOCOL as u64) {
            return self.envelope(echoed_id, echoed_op, None,
                                 Err(GatewayError::invalid(format!(
                                     "protocol must be {GATEWAY_PROTOCOL}"))));
        }
        let Some(request_id) = request_id.filter(|id| !id.is_empty() && id.len() <= 200) else {
            return self.envelope(None, echoed_op, None,
                                 Err(GatewayError::invalid("request_id must be a non-empty string of at most 200 characters")));
        };
        if past_deadline(start, REQUEST_DEADLINE) {
            return self.envelope(Some(&request_id), echoed_op, None,
                                 Err(GatewayError::new("deadline_exceeded", "the request deadline passed before any read ran")));
        }
        let Some(op) = op else {
            return self.envelope(Some(&request_id), None, None,
                                 Err(GatewayError::invalid("op must be a string naming one of the discovered tools")));
        };
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let payload = match op.as_str() {
            "discover" => {
                if params.as_object().is_some_and(|object| !object.is_empty()) {
                    Err(GatewayError::invalid("discover takes no parameters"))
                } else {
                    Ok((self.discover(), None))
                }
            }
            "task_read" => self.task_read(&params),
            "evidence_read" => self.evidence_read(&params, start),
            "batch_read" => self.batch_read(&params, start, &request_id),
            other => Err(GatewayError::invalid(format!(
                "unknown op {other:?}; run discover for the available tools"))),
        };
        // A well-formed response is bounded by construction; the final check is the hard stop behind that.
        let response = self.envelope(Some(&request_id), Some(&op), None, payload);
        if serde_json::to_vec(&response).is_ok_and(|bytes| bytes.len() > MAX_RESPONSE_BYTES) {
            return self.envelope(Some(&request_id), Some(&op), None, Err(GatewayError::new("budget_exhausted",
                "the response would exceed the aggregate payload bound; narrow the request (evidence reads page with a cursor)")));
        }
        response
    }

    /// Entry point for transports that carry raw bytes: the input bound is enforced before any parsing.
    pub fn dispatch_bytes(&self, bytes: &[u8]) -> Value {
        if bytes.len() > MAX_REQUEST_BYTES {
            return self.envelope(None, None, None, Err(GatewayError::new("request_too_large",
                format!("the request is {} bytes; the limit is {MAX_REQUEST_BYTES}", bytes.len()))));
        }
        match serde_json::from_slice::<Value>(bytes) {
            Ok(request) => self.dispatch(&request),
            Err(e) => self.envelope(None, None, None,
                                    Err(GatewayError::invalid(format!("the request is not valid JSON: {e}")))),
        }
    }

    /// The response envelope: protocol, request ID, task revision, and the result or structured error.
    fn envelope(
        &self, request_id: Option<&str>, op: Option<&str>, revision: Option<u64>,
        payload: Result<(Value, Option<u64>), GatewayError>,
    ) -> Value {
        let mut response = json!({
            "protocol": GATEWAY_PROTOCOL,
            "request_id": request_id,
            "op": op,
            "task_revision": revision,
        });
        let map = response.as_object_mut().expect("the envelope is an object");
        match payload {
            Ok((result, revision)) => {
                map.insert("ok".into(), Value::Bool(true));
                if let Some(revision) = revision {
                    map.insert("task_revision".into(), json!(revision));
                }
                map.insert("result".into(), result);
            }
            Err(error) => {
                map.insert("ok".into(), Value::Bool(false));
                map.insert("error".into(), error.to_json());
            }
        }
        response
    }

    /// Compact discovery: the tools, their parameters and the host-policy limits. Kept small on purpose.
    fn discover(&self) -> Value {
        json!({
            "tools": [
                { "name": "discover", "summary": "list the gateway tools, their parameters and the host limits", "params": [] },
                { "name": "task_read", "summary": "read a bounded projection of one task and its registered evidence ids", "params": ["task_id", "task_revision?"] },
                { "name": "evidence_read", "summary": "read a bounded slice of one registered immutable snapshot by opaque evidence id", "params": ["task_id", "evidence_id", "max_bytes?", "offset?", "cursor?", "assignment_id?"] },
                { "name": "batch_read", "summary": "run up to 8 independent task_read/evidence_read steps in one request; steps are not atomic and may be skipped by budget or deadline", "params": ["steps"] },
            ],
            "limits": {
                "batch_steps": MAX_BATCH_STEPS,
                "read_bytes": MAX_READ_BYTES,
                "response_bytes": MAX_RESPONSE_BYTES,
                "deadline_ms": REQUEST_DEADLINE.as_millis() as u64,
                "concurrency": MAX_CONCURRENCY,
            },
        })
    }

    fn load_task(&self, params: &Value) -> Result<(Task, Option<u64>), GatewayError> {
        let task_id = params.get("task_id").and_then(Value::as_str)
            .ok_or_else(|| GatewayError::invalid("task_id is required"))?;
        // A parameter that is present but the wrong type is refused, never ignored: ignoring it would silently
        // read a different revision or scope than the caller asked for.
        let expected = match params.get("task_revision") {
            None => None,
            Some(value) => Some(value.as_u64()
                .ok_or_else(|| GatewayError::invalid("task_revision must be an unsigned integer"))?),
        };
        let task = self.tasks.load(task_id).map_err(|e| match &e {
            crate::task::TaskError::NotFound(_) => GatewayError::new("unknown_task", format!("no task {task_id} exists in this repository")),
            crate::task::TaskError::WrongRepository { .. } => GatewayError::new("wrong_repository", e.to_string()),
            crate::task::TaskError::Corrupt(_) | crate::task::TaskError::UnsupportedVersion(_) =>
                GatewayError::new("corrupt_task", e.to_string()),
            crate::task::TaskError::Invalid(why) => GatewayError::new("invalid_request", why.clone()),
            _ => GatewayError::new("internal", e.to_string()),
        })?;
        if let Some(expected) = expected && task.revision != expected {
            let current = task.revision;
            return Err(GatewayError::new("stale_task_revision",
                format!("the caller expected revision {expected}; the current revision is {current}")));
        }
        let revision = task.revision;
        Ok((task, Some(revision)))
    }

    /// The bounded task projection: owner state and registered evidence identities. Job directories, request
    /// receipts and other private bookkeeping are never part of a response.
    fn task_read(&self, params: &Value) -> Result<(Value, Option<u64>), GatewayError> {
        let (task, revision) = self.load_task(params)?;
        let registered = self.evidence.registered(&task)
            .map_err(|e| GatewayError::new("internal", e.to_string()))?;
        let by_origin = |origin: &str| registered.iter().find(|r| r.origin == origin);
        let describe = |origin: String, reference: String, declared: Option<String>| {
            match by_origin(&origin) {
                // A registered snapshot whose digest no longer matches the current declared identity is stale: it
                // stays readable by its own ID, but it is not presented as satisfying the new declaration. Matching
                // on origin alone would label an old snapshot ready for a re-added reference with a new identity.
                Some(r) if r.digest.is_some()
                    && declared.as_deref().is_some_and(|d| valid_digest(d) && r.digest.as_deref() != Some(d)) => json!({
                        "reference": reference, "evidence_id": Option::<String>::None, "digest": Option::<String>::None,
                        "bytes": Option::<u64>::None, "status": "stale",
                        "detail": "the registered snapshot does not match the declared identity; it remains readable by its own evidence id",
                    }),
                Some(r) => json!({
                    "reference": reference, "evidence_id": r.evidence_id, "digest": r.digest,
                    "bytes": r.bytes, "status": r.status,
                }),
                None => json!({ "reference": reference, "evidence_id": Option::<String>::None, "digest": Option::<String>::None,
                                "bytes": Option::<u64>::None, "status": "unregistered" }),
            }
        };
        let artifacts: Vec<_> = task.artifacts.iter()
            .map(|artifact| describe(artifact_origin(&artifact.reference), artifact.reference.clone(), artifact.identity.clone()))
            .collect();
        let jobs: Vec<_> = task.jobs.iter().map(|job| {
            let evidence: Vec<_> = job.evidence.iter().enumerate().map(|(index, artifact)| {
                let name = match index { 0 => "reply.md", 1 => "events.jsonl", other => return json!({
                    "reference": format!("job:{}/output-{other}", job.id), "evidence_id": Option::<String>::None,
                    "digest": Option::<String>::None, "bytes": Option::<u64>::None, "status": "unregistered",
                }) };
                // A job's evidence entries are trusted only where the recorded reference is that exact output,
                // exactly as registration checks them; an identity assumed from the position alone could label
                // one file with another file's snapshot.
                if Path::new(&artifact.reference) != job.directory.join(name) {
                    return json!({
                        "reference": format!("job:{}/{name}", job.id), "evidence_id": Option::<String>::None,
                        "digest": Option::<String>::None, "bytes": Option::<u64>::None, "status": "unavailable",
                        "detail": "the recorded evidence reference is outside its attached job output",
                    });
                }
                describe(format!("job:{}:{name}", job.id), format!("job:{}/{name}", job.id), artifact.identity.clone())
            }).collect();
            json!({
                "id": job.id, "agent": job.agent, "write": job.write,
                "status_at_attach": job.status_at_attach, "evidence": evidence,
            })
        }).collect();
        let result = json!({
            "task_id": task.id,
            "revision": task.revision,
            "objective": task.objective.text,
            "constraints": task.constraints.iter().map(|c| json!({"text": c.text, "provenance": c.provenance})).collect::<Vec<_>>(),
            "decisions": task.decisions.iter().map(|d| json!({"text": d.text, "status": d.status, "author": d.author})).collect::<Vec<_>>(),
            "next_action": task.next_action.as_ref().map(|n| json!({"text": n.text, "provenance": n.provenance})),
            "unresolved_issues": task.unresolved_issues.iter().map(|i| i.text.clone()).collect::<Vec<_>>(),
            "artifacts": artifacts,
            "jobs": jobs,
        });
        Ok((result, revision))
    }

    /// A bounded slice of one immutable snapshot. The only access path: an opaque evidence ID that resolves, in
    /// this repository, to a registration of this task. Live sources are never read here.
    fn evidence_read(&self, params: &Value, start: Instant) -> Result<(Value, Option<u64>), GatewayError> {
        let (task, revision) = self.load_task(params)?;
        let evidence_id = params.get("evidence_id").and_then(Value::as_str)
            .ok_or_else(|| GatewayError::invalid("evidence_id is required"))?;
        // An agent that names the assignment it was given can be held to exactly the evidence that assignment (or
        // one it builds on) told it about. Without the parameter the read stays task-scoped, which is the host's
        // coarser grant; the assignment is the narrower capability. A present but non-string value is refused: a
        // host must never fall back to the wider scope because the caller's narrower one was malformed.
        let assignment_id = match params.get("assignment_id") {
            None => None,
            Some(Value::String(id)) => Some(id.as_str()),
            Some(_) => return Err(GatewayError::invalid("assignment_id must be a string")),
        };
        if let Some(assignment_id) = assignment_id {
            match self.assignments.allowed_evidence(&task, assignment_id) {
                Ok(Some(allowed)) if allowed.contains(evidence_id) => {}
                Ok(Some(_)) => return Err(GatewayError::new("unknown_evidence",
                    "the evidence id is not part of the named assignment")),
                Ok(None) => return Err(GatewayError::new("unknown_assignment",
                    format!("no assignment {assignment_id} belongs to task {}", task.id))),
                Err(e) => return Err(GatewayError::new("corrupt_assignment", e.to_string())),
            }
        }
        let registration = self.evidence.registration(&task, evidence_id).map_err(|e| match &e {
            crate::evidence::EvidenceError::NotFound(_) => GatewayError::new("unknown_evidence",
                format!("no registered evidence {evidence_id} belongs to task {}", task.id)),
            crate::evidence::EvidenceError::WrongTask(_) => GatewayError::new("unknown_evidence", e.to_string()),
            crate::evidence::EvidenceError::Invalid(why) => GatewayError::new("invalid_request", why.clone()),
            crate::evidence::EvidenceError::Corrupt(why) => GatewayError::new("corrupt_snapshot", why.clone()),
            _ => GatewayError::new("internal", e.to_string()),
        })?;
        let digest = registration.digest.clone().expect("a registered record carries its digest");
        let total = registration.bytes.expect("a registered record carries its size");
        // A cursor continues exactly where its immutable content left off; anything else is stale, never a
        // reinterpretation. An offset is only accepted without a cursor, and a malformed cursor is refused rather
        // than treated as an absent one.
        let cursor = match params.get("cursor") {
            None => None,
            Some(Value::String(cursor)) => Some(cursor.as_str()),
            Some(_) => return Err(GatewayError::invalid("cursor must be a string")),
        };
        let offset = match (cursor, params.get("offset")) {
            (Some(cursor), None) => {
                let decoded = decode_cursor(cursor)?;
                if decoded.v != GATEWAY_PROTOCOL || decoded.task != task.id || decoded.evidence != evidence_id
                    || decoded.digest != digest
                {
                    return Err(GatewayError::new("stale_cursor",
                        "the cursor does not match this task, this evidence id and its immutable content identity"));
                }
                decoded.offset
            }
            (None, offset) => match offset {
                None => 0,
                Some(value) => value.as_u64().ok_or_else(|| {
                    GatewayError::invalid("offset must be an unsigned integer")
                })?,
            },
            (Some(_), Some(_)) => {
                return Err(GatewayError::invalid("a cursor already carries its offset; pass either cursor or offset, not both"))
            }
        };
        if offset > total {
            return Err(GatewayError::new("invalid_request",
                format!("offset {offset} is beyond the {total}-byte snapshot")));
        }
        let mut max_bytes = match params.get("max_bytes") {
            None => MAX_READ_BYTES,
            Some(value) => value.as_u64().and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| GatewayError::invalid("max_bytes must be an unsigned integer"))?,
        };
        // A zero-byte read before the end would return a cursor at the same offset forever: no progress, no error.
        if max_bytes == 0 {
            return Err(GatewayError::invalid("max_bytes must be at least 1; a zero-byte read makes no progress"));
        }
        max_bytes = max_bytes.min(MAX_READ_BYTES);
        if let Some(budget) = self.remaining_single_read_budget() {
            max_bytes = max_bytes.min(budget);
        }
        let mut file = self.open_snapshot(&task, evidence_id)?;
        // Verify the bounded snapshot before slicing it. A same-length corruption would otherwise be served for
        // every page under the original digest: for snapshots larger than one page, no single page covers the whole
        // file, so the whole-content check would never run.
        let mut verified = Vec::new();
        Read::by_ref(&mut file).take(total + 1).read_to_end(&mut verified)
            .map_err(|e| GatewayError::new("missing_snapshot", format!("the snapshot could not be read: {e}")))?;
        if verified.len() as u64 != total {
            return Err(GatewayError::new("corrupt_snapshot", format!(
                "the immutable snapshot is {} bytes; its registration says {total}; the live source is not substituted",
                verified.len())));
        }
        let actual = format!("sha256:{}", hex(&Sha256::digest(&verified)));
        if actual != digest {
            return Err(GatewayError::new("corrupt_snapshot",
                "the snapshot bytes no longer match their registered digest; the live source is not substituted"));
        }
        let start_at = usize::try_from(offset).unwrap_or(usize::MAX).min(verified.len());
        let end_at = start_at.saturating_add(max_bytes).min(verified.len());
        let content = verified[start_at..end_at].to_vec();
        let returned = content.len() as u64;
        let truncated = offset + returned < total;
        let mut result = json!({
            "task_id": task.id,
            "evidence_id": evidence_id,
            "digest": digest,
            "total_bytes": total,
            "offset": offset,
            "bytes_returned": returned,
            "truncated": truncated,
        });
        let map = result.as_object_mut().expect("the result is an object");
        match String::from_utf8(content) {
            Ok(text) => { map.insert("encoding".into(), json!("utf8")); map.insert("content".into(), json!(text)); }
            Err(error) => {
                map.insert("encoding".into(), json!("base64"));
                map.insert("content".into(), json!(b64url_encode(&error.into_bytes())));
            }
        }
        if truncated {
            let next = Cursor { v: GATEWAY_PROTOCOL, task: task.id.clone(), evidence: evidence_id.into(),
                                digest: digest.clone(), offset: offset + returned };
            map.insert("next_cursor".into(), json!(encode_cursor(&next)));
        }
        if past_deadline(start, REQUEST_DEADLINE) {
            // The read finished; the deadline is reported so a host can decide not to continue paging.
            map.insert("deadline_exceeded".into(), Value::Bool(true));
        }
        Ok((result, revision))
    }

    fn open_snapshot(&self, task: &Task, evidence_id: &str) -> Result<fs::File, GatewayError> {
        self.evidence.snapshot(task, evidence_id).map_err(|e| match &e {
            crate::evidence::EvidenceError::NotFound(_) => GatewayError::new("unknown_evidence", e.to_string()),
            crate::evidence::EvidenceError::WrongTask(_) => GatewayError::new("unknown_evidence", e.to_string()),
            crate::evidence::EvidenceError::Invalid(why) => GatewayError::new("invalid_request", why.clone()),
            crate::evidence::EvidenceError::Corrupt(why) => GatewayError::new("corrupt_snapshot", why.clone()),
            _ => GatewayError::new("missing_snapshot",
                "the immutable snapshot could not be opened; it is reported, never replaced by the live source"),
        })
    }

    /// Content room left for a single read: the aggregate bound minus the reserved terminal status and the
    /// envelope this response itself needs.
    fn remaining_single_read_budget(&self) -> Option<usize> {
        let probe = self.envelope(Some(&"x".repeat(24)), Some("evidence_read"), Some(0), Ok((json!({}), None)));
        let framing = serde_json::to_vec(&probe).ok()?.len() + 8;
        MAX_RESPONSE_BYTES.checked_sub(TERMINAL_STATUS_RESERVE + framing)
    }

    /// Independent steps, run in order, sharing one deadline and one payload budget. Steps never depend on each
    /// other, may fail or be skipped individually, and the batch is explicitly not atomic.
    fn batch_read(&self, params: &Value, start: Instant, request_id: &str) -> Result<(Value, Option<u64>), GatewayError> {
        let steps = params.get("steps").and_then(Value::as_array)
            .ok_or_else(|| GatewayError::invalid("steps must be an array of {tool, params} reads"))?
            .clone();
        if steps.len() > MAX_BATCH_STEPS {
            return Err(GatewayError::new("invalid_request",
                format!("a batch runs at most {MAX_BATCH_STEPS} steps; {} were requested", steps.len())));
        }
        // Frame against the actual echoed request ID (a 200-character ID of JSON-escaped control characters is far
        // larger on the wire than a placeholder) and the widest task revision, so a step admitted here can never push
        // the final envelope past the aggregate bound and erase the per-step results. The terminal flags and any skip
        // markers are held back by TERMINAL_STATUS_RESERVE.
        let probe = self.envelope(Some(request_id), Some("batch_read"), None,
                                  Ok((json!({ "results": [] }), Some(u64::MAX))));
        let framing = serde_json::to_vec(&probe).map(|bytes| bytes.len()).unwrap_or(0) + 8;
        let mut remaining = MAX_RESPONSE_BYTES.saturating_sub(TERMINAL_STATUS_RESERVE + framing);
        let mut results = Vec::with_capacity(steps.len());
        let mut revision = None;
        // The deadline is a soft cutoff: a synchronous read cannot be cancelled, so the gateway stops starting
        // steps, marks a step that finished late, and reports the whole batch incomplete. A host that needs hard
        // cancellation bounds the process itself.
        let mut late = false;
        for step in &steps {
            if past_deadline(start, REQUEST_DEADLINE) {
                late = true;
                results.push(skipped(step, "deadline_exceeded"));
                continue;
            }
            if remaining < MIN_STEP_ROOM + RESERVED_STEP_MARKER {
                results.push(skipped(step, "budget_exhausted"));
                continue;
            }
            let tool = step.get("tool").and_then(Value::as_str).unwrap_or_default();
            let step_params = step.get("params").cloned().unwrap_or_else(|| json!({}));
            let outcome = match tool {
                "task_read" => self.task_read(&step_params),
                "evidence_read" => self.evidence_read(&step_params, start),
                "batch_read" => Err(GatewayError::invalid("nested batches are not allowed")),
                "" => Err(GatewayError::invalid("each step names a tool: task_read or evidence_read")),
                other => Err(GatewayError::invalid(format!(
                    "unknown step tool {other:?}; batch steps are task_read or evidence_read only"))),
            };
            let mut step_value = match outcome {
                Ok((result, step_revision)) => {
                    if revision.is_none() { revision = step_revision; }
                    json!({ "tool": tool_label(step), "status": "ok", "result": result })
                }
                Err(error) => json!({ "tool": tool_label(step), "status": "error", "error": error.to_json() }),
            };
            if past_deadline(start, REQUEST_DEADLINE) {
                // The step ran and its result is kept, but it finished past the bound: the consumer is told.
                late = true;
                if let Some(map) = step_value.as_object_mut() {
                    map.insert("deadline_exceeded".into(), Value::Bool(true));
                }
            }
            let cost = serde_json::to_vec(&step_value).map(|bytes| bytes.len() + 1).unwrap_or(usize::MAX);
            if cost > remaining {
                results.push(skipped(step, "budget_exhausted"));
                remaining = remaining.saturating_sub(RESERVED_STEP_MARKER);
                continue;
            }
            remaining -= cost;
            results.push(step_value);
        }
        if past_deadline(start, REQUEST_DEADLINE) {
            late = true;
        }
        let complete = batch_is_complete(&results, late);
        let mut result = json!({ "results": results, "complete": complete });
        if late {
            result.as_object_mut().expect("the batch result is an object")
                .insert("deadline_exceeded".into(), Value::Bool(true));
        }
        Ok((result, revision))
    }
}

/// Room a plain skip marker needs inside the aggregate budget; skip markers must always be affordable. The label is
/// clipped, so the reserve bounds every marker and `TERMINAL_STATUS_RESERVE` holds all of them.
const MAX_STEP_LABEL_CHARS: usize = 24;
const RESERVED_STEP_MARKER: usize = 120;
const _: () = assert!(MAX_BATCH_STEPS * RESERVED_STEP_MARKER <= TERMINAL_STATUS_RESERVE);

/// A bounded, printable label for a step's tool. The request may carry an arbitrarily long unknown name; echoing it
/// whole would let a skipped step's marker crowd out earlier successful results.
fn tool_label(step: &Value) -> String {
    step.get("tool").and_then(Value::as_str).unwrap_or("")
        .chars().take(MAX_STEP_LABEL_CHARS).filter(|c| c.is_ascii_graphic()).collect()
}

fn skipped(step: &Value, reason: &'static str) -> Value {
    json!({ "tool": tool_label(step), "status": "skipped", "reason": reason })
}

/// A batch is complete only when every step succeeded and it finished within the deadline. A late result is kept,
/// but it is reported as incomplete: a synchronous read cannot be cancelled, so the bound is a soft cutoff.
fn batch_is_complete(results: &[Value], late: bool) -> bool {
    !late && results.iter().all(|step| step.get("status") == Some(&json!("ok")))
}

fn past_deadline(start: Instant, deadline: Duration) -> bool {
    start.elapsed() >= deadline
}

/// A pagination cursor. It names immutable content: task, evidence id, digest and byte offset. It is opaque to
/// callers and validated in full before it advances anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    v: u32,
    task: String,
    evidence: String,
    digest: String,
    offset: u64,
}

fn encode_cursor(cursor: &Cursor) -> String {
    let bytes = serde_json::to_vec(cursor).expect("a cursor serializes");
    b64url_encode(&bytes)
}

fn decode_cursor(cursor: &str) -> Result<Cursor, GatewayError> {
    let bytes = b64url_decode(cursor)
        .ok_or_else(|| GatewayError::new("invalid_cursor", "the cursor is not a valid opaque token"))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| GatewayError::new("invalid_cursor", format!("the cursor does not decode: {e}")))
}

fn b64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let packed = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(packed >> 18) as usize & 63] as char);
        out.push(TABLE[(packed >> 12) as usize & 63] as char);
        if chunk.len() > 1 { out.push(TABLE[(packed >> 6) as usize & 63] as char); }
        if chunk.len() > 2 { out.push(TABLE[packed as usize & 63] as char); }
    }
    out
}

fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    let value = |byte: u8| -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a') as u32 + 26),
            b'0'..=b'9' => Some((byte - b'0') as u32 + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut accumulator = 0u32;
    let mut filled = 0u32;
    for byte in text.bytes() {
        if byte == b'=' { break; }
        let bits = value(byte)?;
        accumulator = (accumulator << 6) | bits;
        filled += 6;
        if filled >= 8 {
            filled -= 8;
            out.push((accumulator >> filled) as u8);
        }
    }
    Some(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{EvidenceStore, EvidenceStatus};
    use crate::task::{ArtifactRef, JobRef, TaskStore, TaskUpdate};
    use crate::task_assignment::{AssignmentKind, AssignmentStore, ContextPolicy, PreparedAssignment, Section};
    use serde_json::json;
    use sha2::Sha256;

    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!("ringo-gateway-{name}-{}-{}",
                                                        std::process::id(),
                                                        COUNTER.fetch_add(1, Ordering::SeqCst)));
            fs::create_dir_all(path.join("state")).unwrap();
            fs::create_dir_all(path.join("repo")).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn binding(temp: &Temp) -> RepositoryBinding {
        RepositoryBinding { path: temp.0.join("repo"), git: false }
    }

    fn gateway(temp: &Temp) -> Gateway {
        Gateway::new(&temp.0.join("state"), &binding(temp)).unwrap()
    }

    fn store(temp: &Temp) -> TaskStore {
        TaskStore::new(&temp.0.join("state"), binding(temp).path, false).unwrap()
    }

    fn evidence(temp: &Temp, task: &Task) -> EvidenceStore {
        EvidenceStore::new(&temp.0.join("state"), &task.repository).unwrap()
    }

    fn task_with_artifact(temp: &Temp, name: &str, content: &str) -> Task {
        task_with_bytes(temp, name, content.as_bytes())
    }

    fn task_with_bytes(temp: &Temp, name: &str, content: &[u8]) -> Task {
        let path = temp.0.join("repo").join(name);
        fs::write(&path, content).unwrap();
        let task = store(temp).create(&format!("create-{name}").replace('.', "-"), "an objective").unwrap();
        store(temp).update(&task.id, task.revision, &format!("artifact-{name}").replace('.', "-"),
                           TaskUpdate::AddArtifact(ArtifactRef { reference: name.into(), identity: None })).unwrap()
    }

    fn job_ref(temp: &Temp, folder: &str, reply: &str) -> JobRef {
        let dir = temp.0.join("private").join(folder);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("reply.md"), reply).unwrap();
        let id = hex(&Sha256::digest(dir.as_os_str().as_encoded_bytes()));
        let reference = dir.join("reply.md").to_string_lossy().into_owned();
        JobRef {
            id, shell_id: 1, agent: "claude".into(),
            write: false, directory: dir, status_at_attach: "done".into(), base: None, before: None, after: None,
            evidence: [ArtifactRef { reference, identity: None }].into_iter().collect(),
        }
    }

    fn ok(response: &Value) -> &Value {
        assert_eq!(response["ok"], json!(true), "response was: {response}");
        &response["result"]
    }

    fn error_code(response: &Value) -> &str {
        assert_eq!(response["ok"], json!(false), "response was: {response}");
        response["error"]["code"].as_str().expect("an error code")
    }

    fn request(op: &str, params: Value) -> Value {
        json!({ "protocol": GATEWAY_PROTOCOL, "request_id": "req-test", "op": op, "params": params })
    }

    #[test]
    fn discovery_is_compact_and_reports_the_host_limits() {
        let temp = Temp::new("discover");
        let response = gateway(&temp).dispatch(&request("discover", json!({})));
        let result = ok(&response);
        assert_eq!(result["tools"].as_array().unwrap().len(), 4);
        assert_eq!(result["limits"], json!({
            "batch_steps": 8, "read_bytes": 16_384, "response_bytes": 32_768,
            "deadline_ms": 10_000, "concurrency": 2,
        }));
        let bytes = serde_json::to_vec(&response).unwrap().len();
        assert!(bytes < 2_000, "discovery must stay compact: {bytes} bytes");
        assert_eq!(response["task_revision"], json!(null));
        // discovery takes no parameters
        assert_eq!(error_code(&gateway(&temp).dispatch(&request("discover", json!({"x": 1})))), "invalid_request");
    }

    #[test]
    fn task_read_projects_owner_state_and_evidence_ids_without_private_paths() {
        let temp = Temp::new("project");
        let task = task_with_artifact(&temp, "note.txt", "artifact content");
        let job = job_ref(&temp, "job", "the attached reply");
        let task = store(&temp).attach(&task.id, task.revision, "attach", job).unwrap();
        let registered = evidence(&temp, &task).register_task(&task).unwrap();
        assert!(registered.iter().any(|r| r.status == EvidenceStatus::Ready));
        let response = gateway(&temp).dispatch(&request("task_read", json!({ "task_id": task.id })));
        let result = ok(&response);
        assert_eq!(result["revision"], json!(task.revision));
        assert_eq!(result["objective"], json!("an objective"));
        assert_eq!(result["artifacts"][0]["status"], json!("ready"));
        assert!(result["artifacts"][0]["evidence_id"].as_str().unwrap().starts_with("e-"));
        let reference = result["jobs"][0]["evidence"][0]["reference"].as_str().unwrap();
        assert!(reference.ends_with("/reply.md") && reference.starts_with("job:"), "{reference}");
        let text = serde_json::to_string(&response).unwrap();
        assert!(!text.contains(&temp.0.join("private").display().to_string()), "no private job paths: {text}");
        assert!(!text.contains("directory"), "no directory keys: {text}");
        assert!(!text.contains(&temp.0.join("repo").display().to_string()), "no raw repository paths: {text}");
        assert_eq!(response["task_revision"], json!(task.revision));
    }

    #[test]
    fn task_read_reports_a_redeclared_artifact_stale_not_ready_with_the_old_snapshot() {
        // The owner removes and re-adds the same reference with a different declared identity, without a new
        // registration. Matching on origin alone would label the old snapshot ready for the new declaration.
        let temp = Temp::new("redeclare");
        let task = task_with_artifact(&temp, "note.txt", "original bytes");
        let old_id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let store = store(&temp);
        let task = store.update(&task.id, task.revision, "rm", TaskUpdate::RemoveArtifact(1)).unwrap();
        let wrong = format!("sha256:{}", "ab".repeat(32));
        let task = store.update(&task.id, task.revision, "add",
            TaskUpdate::AddArtifact(ArtifactRef { reference: "note.txt".into(), identity: Some(wrong) })).unwrap();
        let response = gateway(&temp).dispatch(&request("task_read", json!({ "task_id": task.id })));
        let result = ok(&response);
        let artifact = &result["artifacts"][0];
        assert_eq!(artifact["status"], json!("stale"), "{artifact}");
        assert_eq!(artifact["evidence_id"], json!(null), "{artifact}");
        assert_eq!(artifact["digest"], json!(null), "{artifact}");
        // The old snapshot remains readable by its own ID, but it is not presented as satisfying the new declaration.
        let read = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": old_id })));
        assert_eq!(ok(&read)["content"], json!("original bytes"));
    }

    #[test]
    fn an_evidence_read_pages_through_a_cursor_bound_to_the_content() {
        let temp = Temp::new("page");
        let task = task_with_artifact(&temp, "big.txt", "0123456789".repeat(1000).as_str());
        let registered = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap();
        let id = registered.evidence_id.unwrap().clone();
        let g = gateway(&temp);
        let mut result = ok(&g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "max_bytes": 4096 })))).clone();
        assert_eq!(result["bytes_returned"], json!(4096));
        assert_eq!(result["truncated"], json!(true));
        assert_eq!(result["total_bytes"], json!(10_000));
        let mut gathered = String::new();
        gathered.push_str(result["content"].as_str().unwrap());
        while result["truncated"] == json!(true) {
            let cursor = result["next_cursor"].as_str().unwrap().to_owned();
            result = ok(&g.dispatch(&request("evidence_read",
                json!({ "task_id": task.id, "evidence_id": id, "cursor": cursor })))).clone();
            gathered.push_str(result["content"].as_str().unwrap());
        }
        assert_eq!(gathered, "0123456789".repeat(1000));
        // a continuation without max_bytes reads to the end within the default read bound
        assert_eq!(result["bytes_returned"], json!(10_000 - 4096));
        // a whole-snapshot read verifies its own digest
        let whole = ok(&g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "max_bytes": 20_000 })))).clone();
        assert_eq!(whole["truncated"], json!(false));
        // replaying a cursor returns the same page again
        let replay = ok(&g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "max_bytes": 4096 })))).clone();
        assert_eq!(replay["content"], ok(&g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "max_bytes": 4096 }))))["content"]);
    }

    #[test]
    fn a_cursor_bound_to_other_content_or_scoped_elsewhere_is_stale() {
        let temp = Temp::new("stale");
        let task = task_with_artifact(&temp, "note.txt", "some content");
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let forged = encode_cursor(&Cursor {
            v: GATEWAY_PROTOCOL, task: task.id.clone(), evidence: id.clone(),
            digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(), offset: 4,
        });
        let response = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "cursor": forged })));
        assert_eq!(error_code(&response), "stale_cursor");
        // a cursor naming another task
        let other = encode_cursor(&Cursor {
            v: GATEWAY_PROTOCOL, task: "t-other".into(), evidence: id.clone(),
            digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(), offset: 0,
        });
        let response = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "cursor": other })));
        assert_eq!(error_code(&response), "stale_cursor");
        // an undecodable token
        let response = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "cursor": "not a token" })));
        assert_eq!(error_code(&response), "invalid_cursor");
        // cursor and offset together are refused
        let response = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "cursor": forged, "offset": 0 })));
        assert_eq!(error_code(&response), "invalid_request");
    }

    #[test]
    fn cross_task_ids_unknown_tasks_and_missing_registrations_are_refused() {
        let temp = Temp::new("scope");
        let task = task_with_artifact(&temp, "note.txt", "content");
        let other = store(&temp).create("other", "another objective").unwrap();
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let g = gateway(&temp);
        // another task of the same repository may not read this evidence
        let response = g.dispatch(&request("evidence_read",
            json!({ "task_id": other.id, "evidence_id": id })));
        assert_eq!(error_code(&response), "unknown_evidence");
        // unknown task: well-formed but absent
        let response = g.dispatch(&request("task_read", json!({ "task_id": "t-000000000000000000000000" })));
        assert_eq!(error_code(&response), "unknown_task");
        // a malformed task id is a request problem, not a store problem
        let response = g.dispatch(&request("task_read", json!({ "task_id": "t-missing" })));
        assert_eq!(error_code(&response), "invalid_request");
        // unregistered artifact: task_read lists it without an id, and evidence_read refuses a bare reference
        let bare = g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": "note.txt" })));
        assert_eq!(error_code(&bare), "invalid_request");
        let response = g.dispatch(&request("task_read", json!({ "task_id": other.id })));
        let listing = ok(&response);
        assert_eq!(listing["artifacts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_deleted_live_source_still_reads_from_its_snapshot() {
        let temp = Temp::new("snapshot");
        let task = task_with_artifact(&temp, "gone.txt", "the original bytes");
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        fs::remove_file(temp.0.join("repo/gone.txt")).unwrap();
        fs::write(temp.0.join("repo/gone.txt"), "totally different live content").unwrap();
        let response = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id })));
        assert_eq!(ok(&response)["content"], json!("the original bytes"));
        assert_eq!(ok(&response)["digest"].as_str().unwrap().len(), "sha256:".len() + 64);
    }

    #[test]
    fn a_missing_snapshot_is_reported_and_never_replaced_by_the_live_source() {
        let temp = Temp::new("missing");
        let task = task_with_artifact(&temp, "note.txt", "content");
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let blob = temp.0.join("state/task-evidence");
        for entry in fs::read_dir(&blob).unwrap().collect::<Vec<_>>() {
            let _ = fs::remove_dir_all(entry.unwrap().path().join(&task.id).join("blobs"));
        }
        let response = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id })));
        assert_eq!(error_code(&response), "missing_snapshot");
    }

    #[test]
    fn a_stale_expected_revision_is_refused_with_the_current_one() {
        let temp = Temp::new("revision");
        let task = task_with_artifact(&temp, "note.txt", "content");
        let response = gateway(&temp).dispatch(&request("task_read",
            json!({ "task_id": task.id, "task_revision": task.revision + 1 })));
        assert_eq!(error_code(&response), "stale_task_revision");
        assert!(response["error"]["detail"].as_str().unwrap_or_default().contains(&task.revision.to_string()));
    }

    #[test]
    fn batches_run_independent_steps_and_report_partial_completion() {
        let temp = Temp::new("batch");
        let task = task_with_artifact(&temp, "note.txt", "batch content");
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let other = store(&temp).create("other", "another").unwrap();
        let steps = json!([
            { "tool": "task_read", "params": { "task_id": task.id } },
            { "tool": "evidence_read", "params": { "task_id": other.id, "evidence_id": id } },
            { "tool": "evidence_read", "params": { "task_id": task.id, "evidence_id": id, "max_bytes": 5 } },
        ]);
        let response = gateway(&temp).dispatch(&request("batch_read", json!({ "steps": steps })));
        let result = ok(&response);
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["status"], json!("ok"));
        assert_eq!(results[1]["status"], json!("error"));
        assert_eq!(results[1]["error"]["code"], json!("unknown_evidence"));
        assert_eq!(results[2]["status"], json!("ok"));
        assert_eq!(result["complete"], json!(false));
        assert_eq!(results[2]["result"]["bytes_returned"], json!(5));
        // a clean batch is complete
        let clean = json!([
            { "tool": "task_read", "params": { "task_id": task.id } },
            { "tool": "evidence_read", "params": { "task_id": task.id, "evidence_id": id, "max_bytes": 4 } },
        ]);
        let response = gateway(&temp).dispatch(&request("batch_read", json!({ "steps": clean })));
        assert_eq!(ok(&response)["complete"], json!(true));
    }

    #[test]
    fn batch_structure_is_bounded_and_nested_batches_are_refused() {
        let temp = Temp::new("structure");
        let task = task_with_artifact(&temp, "note.txt", "content");
        let steps: Vec<_> = (0..MAX_BATCH_STEPS + 1).map(|_| json!({
            "tool": "task_read", "params": { "task_id": task.id },
        })).collect();
        let response = gateway(&temp).dispatch(&request("batch_read", json!({ "steps": steps })));
        assert_eq!(error_code(&response), "invalid_request");
        let nested = json!([{ "tool": "batch_read", "params": { "steps": [] } }]);
        let response = gateway(&temp).dispatch(&request("batch_read", json!({ "steps": nested })));
        let result = ok(&response);
        assert_eq!(result["results"][0]["status"], json!("error"));
        assert_eq!(result["results"][0]["error"]["code"], json!("invalid_request"));
        let unknown = json!([{ "tool": "evidence_write", "params": {} }]);
        let response = gateway(&temp).dispatch(&request("batch_read", json!({ "steps": unknown })));
        assert_eq!(ok(&response)["results"][0]["error"]["code"], json!("invalid_request"));
        let response = gateway(&temp).dispatch(&request("batch_read", json!({ "steps": [] })));
        assert_eq!(ok(&response)["complete"], json!(true));
        // steps must be an array
        assert_eq!(error_code(&gateway(&temp).dispatch(&request("batch_read", json!({ "steps": "no" })))), "invalid_request");
    }

    #[test]
    fn a_batch_that_fills_the_payload_budget_skips_the_rest_still_under_the_bound() {
        let temp = Temp::new("budget");
        let task = task_with_artifact(&temp, "big.txt", &"x".repeat(20_000));
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let steps: Vec<_> = (0..MAX_BATCH_STEPS).map(|_| json!({
            "tool": "evidence_read", "params": { "task_id": task.id, "evidence_id": id },
        })).collect();
        let response = gateway(&temp).dispatch(&request("batch_read", json!({ "steps": steps })));
        let result = ok(&response);
        let results = result["results"].as_array().unwrap();
        let executed = results.iter().filter(|s| s["status"] == json!("ok")).count();
        let skipped = results.iter().filter(|s| s["status"] == json!("skipped")).count();
        assert!((1..MAX_BATCH_STEPS).contains(&executed), "executed {executed}");
        assert_eq!(executed + skipped, MAX_BATCH_STEPS);
        assert_eq!(result["complete"], json!(false));
        assert_eq!(results.last().unwrap()["reason"], json!("budget_exhausted"));
        let bytes = serde_json::to_vec(&response).unwrap().len();
        assert!(bytes <= MAX_RESPONSE_BYTES, "the response must respect the aggregate bound: {bytes}");
    }

    #[test]
    fn a_long_unknown_tool_name_cannot_erase_earlier_successful_steps() {
        // A request within the input bound can carry a huge unknown tool name. Echoing it into a step result would
        // push the whole response past the aggregate bound, and the final hard stop would throw away the earlier
        // successful page instead of reporting the bad step.
        let temp = Temp::new("toolname");
        let task = task_with_artifact(&temp, "big.txt", &"x".repeat(20_000));
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let steps = json!([
            { "tool": "evidence_read", "params": { "task_id": task.id, "evidence_id": id } },
            { "tool": "z".repeat(15_900), "params": {} },
        ]);
        let request = request("batch_read", json!({ "steps": steps }));
        assert!(serde_json::to_vec(&request).unwrap().len() <= MAX_REQUEST_BYTES);
        let response = gateway(&temp).dispatch(&request);
        let result = ok(&response);
        let results = result["results"].as_array().unwrap();
        assert_eq!(results[0]["status"], json!("ok"), "{response}");
        assert_eq!(results[0]["tool"], json!("evidence_read"));
        assert_eq!(results[0]["result"]["bytes_returned"], json!(MAX_READ_BYTES as u64));
        assert_eq!(results[1]["status"], json!("error"));
        assert_eq!(results[1]["tool"].as_str().unwrap().len(), MAX_STEP_LABEL_CHARS);
        assert_eq!(result["complete"], json!(false));
        let bytes = serde_json::to_vec(&response).unwrap().len();
        assert!(bytes <= MAX_RESPONSE_BYTES, "the response must respect the aggregate bound: {bytes}");
    }

    #[test]
    fn a_long_escaped_request_id_cannot_erase_successful_batch_steps() {
        // A valid 200-character request ID of control characters serializes to ~1200 bytes of \uXXXX escapes. The
        // batch must frame against that actual ID, not a short placeholder, or an admitted step would push the final
        // envelope past the bound and the hard stop would replace the per-step results with a top-level error.
        let temp = Temp::new("escaped-id");
        let task = task_with_artifact(&temp, "big.txt", &"x".repeat(20_000));
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let steps = json!([
            { "tool": "evidence_read", "params": { "task_id": task.id, "evidence_id": id } },
            { "tool": "evidence_read", "params": { "task_id": task.id, "evidence_id": id } },
            { "tool": "evidence_read", "params": { "task_id": task.id, "evidence_id": id } },
        ]);
        let request_id = "\u{1}".repeat(200);
        let request = json!({ "protocol": GATEWAY_PROTOCOL, "request_id": request_id, "op": "batch_read",
                              "params": { "steps": steps } });
        assert!(serde_json::to_vec(&request).unwrap().len() <= MAX_REQUEST_BYTES);
        let response = gateway(&temp).dispatch(&request);
        let result = ok(&response);
        let results = result["results"].as_array().unwrap();
        assert_eq!(results[0]["status"], json!("ok"), "{response}");
        assert_eq!(results[0]["result"]["bytes_returned"], json!(MAX_READ_BYTES as u64));
        assert_eq!(response["request_id"], json!(request_id));
        let bytes = serde_json::to_vec(&response).unwrap().len();
        assert!(bytes <= MAX_RESPONSE_BYTES, "the response must respect the aggregate bound: {bytes}");
    }


    #[test]
    fn only_one_read_step_worth_of_content_fits_each_step_and_the_cap_holds() {
        let temp = Temp::new("readcap");
        let task = task_with_artifact(&temp, "big.txt", &"y".repeat(30_000));
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let response = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id })));
        assert_eq!(ok(&response)["bytes_returned"], json!(MAX_READ_BYTES as u64));
        // an oversized max_bytes request is clamped, not refused
        let response = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "max_bytes": 100_000 })));
        assert_eq!(ok(&response)["bytes_returned"], json!(MAX_READ_BYTES as u64));
        // single reads stay under the aggregate envelope bound
        let bytes = serde_json::to_vec(&response).unwrap().len();
        assert!(bytes <= MAX_RESPONSE_BYTES, "{bytes}");
    }

    #[test]
    fn excess_concurrent_requests_are_refused_not_queued() {
        let temp = Temp::new("busy");
        let g = gateway(&temp);
        let first = g.try_admit().expect("the first slot");
        let second = g.try_admit().expect("the second slot");
        assert!(g.try_admit().is_none(), "the limit is 2");
        let response = g.dispatch(&request("discover", json!({})));
        assert_eq!(error_code(&response), "busy");
        drop(second);
        let response = g.dispatch(&request("discover", json!({})));
        assert_eq!(response["ok"], json!(true));
        drop(first);
        assert!(g.try_admit().is_some());
    }

    #[test]
    fn oversized_and_malformed_requests_are_refused_without_execution() {
        let temp = Temp::new("oversize");
        let g = gateway(&temp);
        let large = json!({ "protocol": GATEWAY_PROTOCOL, "request_id": "r", "op": "discover",
                            "params": { "filler": "z".repeat(MAX_REQUEST_BYTES) } });
        let response = g.dispatch(&large);
        assert_eq!(error_code(&response), "request_too_large");
        let response = g.dispatch_bytes(&vec![0u8; MAX_REQUEST_BYTES + 1]);
        assert_eq!(error_code(&response), "request_too_large");
        let response = g.dispatch_bytes(b"not json at all");
        assert_eq!(error_code(&response), "invalid_request");
        // protocol mismatch
        let response = g.dispatch(&json!({ "protocol": 99, "request_id": "r", "op": "discover", "params": {} }));
        assert_eq!(error_code(&response), "invalid_request");
        // missing request id
        let response = g.dispatch(&json!({ "protocol": GATEWAY_PROTOCOL, "op": "discover", "params": {} }));
        assert_eq!(error_code(&response), "invalid_request");
        // unknown op
        let response = g.dispatch(&request("evidence_write", json!({})));
        assert_eq!(error_code(&response), "invalid_request");
    }

    #[test]
    fn a_past_deadline_is_detected_and_reported() {
        let start = Instant::now() - REQUEST_DEADLINE - Duration::from_secs(1);
        assert!(past_deadline(start, REQUEST_DEADLINE));
        assert!(!past_deadline(Instant::now(), REQUEST_DEADLINE));
    }

    #[test]
    fn cursors_survive_a_gateway_restart_and_still_name_the_same_content() {
        let temp = Temp::new("restart");
        let task = task_with_artifact(&temp, "note.txt", "restart me in pages");
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let first = ok(&gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "max_bytes": 6 })))).clone();
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();
        // a fresh gateway over the same state: identity and cursors remain valid
        let continued = ok(&gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "cursor": cursor })))).clone();
        assert_eq!(format!("{}{}", first["content"].as_str().unwrap(), continued["content"].as_str().unwrap()),
                   "restart me in pages");
        assert_eq!(continued["truncated"], json!(false));
    }

    #[test]
    fn binary_snapshots_travel_as_base64_and_text_as_utf8() {
        let temp = Temp::new("binary");
        let task = task_with_bytes(&temp, "bin.dat", &[0u8, 159, 146, 150, b'a']);
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let response = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id })));
        let result = ok(&response);
        assert_eq!(result["encoding"], json!("base64"));
        assert_eq!(b64url_decode(result["content"].as_str().unwrap()).unwrap(), vec![0u8, 159, 146, 150, b'a']);
        let task = task_with_artifact(&temp, "text.txt", "plain text ✓");
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let response = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id })));
        assert_eq!(ok(&response)["encoding"], json!("utf8"));
        assert_eq!(ok(&response)["content"], json!("plain text ✓"));
    }

    #[test]
    fn offset_reads_without_a_cursor_are_bounded_and_checked() {
        let temp = Temp::new("offset");
        let task = task_with_artifact(&temp, "note.txt", "0123456789");
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let g = gateway(&temp);
        let response = g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "offset": 4, "max_bytes": 3 })));
        let result = ok(&response);
        assert_eq!(result["offset"], json!(4));
        assert_eq!(result["bytes_returned"], json!(3));
        assert_eq!(result["truncated"], json!(true));
        // beyond the end
        let response = g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "offset": 100 })));
        assert_eq!(error_code(&response), "invalid_request");
        // a negative or fractional offset is refused
        let response = g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "offset": -1 })));
        assert_eq!(error_code(&response), "invalid_request");
    }

    fn assigned_evidence(prepared: &PreparedAssignment, reference: &str) -> String {
        match prepared.manifest.required.artifacts.as_ref().expect("a full assignment names its artifacts") {
            Section::Complete(registrations) => registrations.iter()
                .find(|r| r.reference == reference).expect("the artifact is named").evidence_id.clone().unwrap(),
            Section::Changes { .. } => panic!("a full assignment is complete"),
        }
    }

    #[test]
    fn a_removed_earlier_artifact_never_rebinds_a_later_one_to_its_snapshot() {
        let temp = Temp::new("reindex");
        let task = task_with_artifact(&temp, "first.txt", "first bytes");
        fs::write(temp.0.join("repo/second.txt"), "second bytes").unwrap();
        let task = store(&temp).update(&task.id, task.revision, "second-artifact",
                                      TaskUpdate::AddArtifact(ArtifactRef { reference: "second.txt".into(), identity: None })).unwrap();
        let registered = evidence(&temp, &task).register_task(&task).unwrap();
        let id_of = |name: &str| registered.iter().find(|r| r.reference == name).unwrap().evidence_id.clone().unwrap();
        let (first, second) = (id_of("first.txt"), id_of("second.txt"));
        // Removing the earlier artifact shifts the later one to index 0, with no new registration in between.
        let task = store(&temp).update(&task.id, task.revision, "remove-first", TaskUpdate::RemoveArtifact(1)).unwrap();
        let g = gateway(&temp);
        let result = ok(&g.dispatch(&request("task_read", json!({ "task_id": task.id })))).clone();
        assert_eq!(result["artifacts"].as_array().unwrap().len(), 1);
        assert_eq!(result["artifacts"][0]["reference"], json!("second.txt"));
        assert_eq!(result["artifacts"][0]["evidence_id"], json!(second));
        assert_ne!(result["artifacts"][0]["evidence_id"], json!(first));
        // The removed artifact's immutable snapshot stays readable by its own id.
        let response = g.dispatch(&request("evidence_read", json!({ "task_id": task.id, "evidence_id": first })));
        assert_eq!(ok(&response)["content"], json!("first bytes"));
    }

    #[test]
    fn an_evidence_read_named_to_an_assignment_is_limited_to_its_chain() {
        let temp = Temp::new("grant");
        let task = task_with_artifact(&temp, "spec.md", "spec bytes");
        let assignments = AssignmentStore::new(&temp.0.join("state"), &task.repository).unwrap();
        let full = assignments.prepare(&task, "claude", None, ContextPolicy::bounded()).unwrap().unwrap();
        let spec = assigned_evidence(&full, "spec.md");
        // A later artifact is registered by a later assignment, so the earlier one never granted it.
        fs::write(temp.0.join("repo/later.md"), "later bytes").unwrap();
        let task = store(&temp).update(&task.id, task.revision, "later-artifact",
                                      TaskUpdate::AddArtifact(ArtifactRef { reference: "later.md".into(), identity: None })).unwrap();
        let later = assignments.prepare(&task, "claude", None, ContextPolicy::bounded()).unwrap().unwrap();
        let later_id = assigned_evidence(&later, "later.md");
        let g = gateway(&temp);
        let allowed = g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": spec, "assignment_id": full.manifest.assignment_id })));
        assert_eq!(ok(&allowed)["content"], json!("spec bytes"));
        let outside = g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": later_id, "assignment_id": full.manifest.assignment_id })));
        assert_eq!(error_code(&outside), "unknown_evidence");
        // An unknown or malformed assignment id is refused, never widened to the whole task.
        for bogus in ["a-000000000000000000000000", "spec.md"] {
            let response = g.dispatch(&request("evidence_read",
                json!({ "task_id": task.id, "evidence_id": spec, "assignment_id": bogus })));
            assert_eq!(error_code(&response), "unknown_assignment");
        }
        // A delta builds on an acknowledged assignment: evidence the delta does not repeat stays readable.
        assignments.acknowledge(&task, "claude", "thread-1", &full.manifest.assignment_id).unwrap();
        let task = store(&temp).update(&task.id, task.revision, "grant-constraint",
                                      TaskUpdate::AddConstraint("Keep the grant narrow.".into())).unwrap();
        let delta = assignments.prepare(&task, "claude", Some("thread-1"), ContextPolicy::bounded()).unwrap().unwrap();
        assert_eq!(delta.manifest.kind, AssignmentKind::Delta);
        let response = g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": spec, "assignment_id": delta.manifest.assignment_id })));
        assert_eq!(ok(&response)["content"], json!("spec bytes"));
        // The batch path carries the same scope: a step outside the grant fails alone.
        let request = request("batch_read", json!({ "steps": [
            { "tool": "evidence_read", "params": { "task_id": task.id, "evidence_id": spec,
                                                   "assignment_id": delta.manifest.assignment_id } },
            { "tool": "evidence_read", "params": { "task_id": task.id, "evidence_id": spec,
                                                   "assignment_id": "a-000000000000000000000000" } },
        ] }));
        let response = g.dispatch(&request);
        let results = ok(&response)["results"].as_array().unwrap().clone();
        assert_eq!(results[0]["status"], json!("ok"));
        assert_eq!(results[1]["error"]["code"], json!("unknown_assignment"));
    }

    #[test]
    fn a_late_request_is_refused_and_late_batch_steps_are_skipped_without_running() {
        let temp = Temp::new("deadline");
        let task = task_with_artifact(&temp, "note.txt", "deadline content");
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let g = gateway(&temp);
        let late = Instant::now() - REQUEST_DEADLINE - Duration::from_secs(1);
        let response = g.dispatch_at(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id })), late);
        assert_eq!(error_code(&response), "deadline_exceeded");
        // A batch entered late skips every step, still reporting its exact shape and why.
        let (result, _) = g.batch_read(&json!({ "steps": [
            { "tool": "task_read", "params": { "task_id": task.id } },
            { "tool": "evidence_read", "params": { "task_id": task.id, "evidence_id": id } },
        ] }), late, "req-late").unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|s| s["status"] == json!("skipped") && s["reason"] == json!("deadline_exceeded")));
        assert_eq!(result["complete"], json!(false));
        // A read that finishes after the deadline still returns its bounded content and says it was late.
        let (result, _) = g.evidence_read(&json!({ "task_id": task.id, "evidence_id": id }), late).unwrap();
        assert_eq!(result["content"], json!("deadline content"));
        assert_eq!(result["deadline_exceeded"], json!(true));
    }

    #[test]
    fn a_job_evidence_entry_outside_its_output_is_not_given_an_identity() {
        let temp = Temp::new("jobref");
        let mut job = job_ref(&temp, "job", "the real reply");
        fs::write(temp.0.join("private/secret.txt"), "not the job's output").unwrap();
        job.evidence[0].reference = temp.0.join("private/secret.txt").to_string_lossy().into_owned();
        let task = store(&temp).create("jobref-create", "an objective").unwrap();
        let task = store(&temp).attach(&task.id, task.revision, "jobref-attach", job).unwrap();
        let registered = evidence(&temp, &task).register_task(&task).unwrap();
        assert!(registered.iter().all(|r| r.evidence_id.is_none()), "no snapshot is issued for a displaced reference");
        let result = ok(&gateway(&temp).dispatch(&request("task_read", json!({ "task_id": task.id })))).clone();
        assert_eq!(result["jobs"][0]["evidence"][0]["status"], json!("unavailable"));
        assert!(result["jobs"][0]["evidence"][0]["evidence_id"].is_null());
    }

    #[test]
    fn a_present_but_malformed_scope_parameter_is_refused_not_widened_or_ignored() {
        let temp = Temp::new("malformed");
        let task = task_with_artifact(&temp, "note.txt", "content");
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let g = gateway(&temp);
        // A null or numeric assignment is refused: it must not fall back to the wider task scope.
        for scope in [json!(null), json!(7), json!(["a-000000000000000000000000"])] {
            let response = g.dispatch(&request("evidence_read",
                json!({ "task_id": task.id, "evidence_id": id, "assignment_id": scope })));
            assert_eq!(error_code(&response), "invalid_request", "{response}");
        }
        // A malformed revision is refused rather than skipped, so a caller cannot lose a revision check.
        let response = g.dispatch(&request("task_read", json!({ "task_id": task.id, "task_revision": "7" })));
        assert_eq!(error_code(&response), "invalid_request");
        // A cursor of the wrong type is refused rather than read as an absent cursor.
        let response = g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "cursor": 5 })));
        assert_eq!(error_code(&response), "invalid_request");
    }

    #[test]
    fn a_shortened_or_zero_length_read_is_reported_instead_of_paging_forever() {
        let temp = Temp::new("short");
        let task = task_with_artifact(&temp, "note.txt", "0123456789");
        let registered = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap();
        let id = registered.evidence_id.unwrap();
        let g = gateway(&temp);
        // A zero-byte page would return the same cursor at the same offset forever.
        let response = g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "max_bytes": 0 })));
        assert_eq!(error_code(&response), "invalid_request");
        // Shorten the immutable snapshot on disk: the registered length is the truth, not the file.
        let blobs = snapshots(&temp.0.join("state"));
        assert_eq!(blobs.len(), 1, "{blobs:?}");
        fs::write(&blobs[0], b"01234").unwrap();
        let response = g.dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "max_bytes": 5 })));
        assert_eq!(error_code(&response), "corrupt_snapshot", "{response}");
        let response = g.dispatch(&request("evidence_read", json!({ "task_id": task.id, "evidence_id": id })));
        assert_eq!(error_code(&response), "corrupt_snapshot");
    }

    #[test]
    fn a_same_length_snapshot_corruption_is_refused_even_on_a_page_that_does_not_cover_it() {
        let temp = Temp::new("tamper");
        // A snapshot larger than one read page: no single page covers the whole file.
        let task = task_with_artifact(&temp, "big.txt", &"a".repeat(20_000));
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let blobs = snapshots(&temp.0.join("state"));
        assert_eq!(blobs.len(), 1, "{blobs:?}");
        // Corrupt the last byte in place: the length is unchanged, only the digest differs.
        let mut bytes = fs::read(&blobs[0]).unwrap();
        let last = bytes.len() - 1;
        bytes[last] = b'b';
        fs::write(&blobs[0], &bytes).unwrap();
        // The first page does not include the changed byte; it must still not be served under the original digest.
        let response = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id, "max_bytes": 4096 })));
        assert_eq!(error_code(&response), "corrupt_snapshot", "{response}");
        assert!(response["error"]["detail"].as_str().unwrap_or_default().contains("digest"));
    }

    #[test]
    fn a_record_declaring_an_oversized_length_is_corrupt_not_a_panic() {
        // A record whose `bytes` exceeds the registration limit must be refused as corrupt rather than driving
        // `take(length + 1)` through an absurd allocation. The snapshot bytes are unchanged; only the metadata lies.
        let temp = Temp::new("oversize-record");
        let task = task_with_artifact(&temp, "note.txt", "content");
        let id = evidence(&temp, &task).register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let record_path = records(&temp.0.join("state"))[0].clone();
        let mut value: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        value["bytes"] = json!(u64::MAX);
        fs::write(&record_path, serde_json::to_vec(&value).unwrap()).unwrap();
        let response = gateway(&temp).dispatch(&request("evidence_read",
            json!({ "task_id": task.id, "evidence_id": id })));
        assert_eq!(error_code(&response), "corrupt_snapshot", "{response}");
    }

    /// Every registered snapshot file under a state root.
    fn snapshots(root: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "bin") {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    fn records(root: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "json")
                    && path.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "records") {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn an_oversized_parsed_request_echoes_no_unbounded_field() {
        let temp = Temp::new("echo");
        let g = gateway(&temp);
        let huge = "x".repeat(40 * 1024);
        for request in [
            json!({ "protocol": GATEWAY_PROTOCOL, "request_id": huge, "op": "discover", "params": {} }),
            json!({ "protocol": GATEWAY_PROTOCOL, "request_id": "r", "op": huge, "params": {} }),
        ] {
            let response = g.dispatch(&request);
            assert_eq!(error_code(&response), "request_too_large");
            let bytes = serde_json::to_vec(&response).unwrap();
            assert!(bytes.len() <= MAX_RESPONSE_BYTES, "the refusal broke its own bound: {} bytes", bytes.len());
        }
        // An oversized-but-parseable request with a valid id still keeps the refusal bounded.
        let response = g.dispatch(&json!({
            "protocol": GATEWAY_PROTOCOL, "request_id": "r", "op": "discover",
            "params": { "filler": "z".repeat(MAX_REQUEST_BYTES) },
        }));
        assert_eq!(error_code(&response), "request_too_large");
        assert_eq!(response["request_id"], json!("r"));
    }

    #[test]
    fn a_late_batch_step_is_reported_and_the_batch_is_not_complete() {
        // The completion rule itself: every step succeeded, but the batch ran past its bound.
        let results = vec![json!({ "tool": "task_read", "status": "ok", "result": {} })];
        assert!(!batch_is_complete(&results, true));
        assert!(batch_is_complete(&results, false));
        let failed = vec![json!({ "tool": "task_read", "status": "skipped", "reason": "deadline_exceeded" })];
        assert!(!batch_is_complete(&failed, false));
    }
}
