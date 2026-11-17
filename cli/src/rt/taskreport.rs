//! Rust port of `taskreport.py`: the whole-task, read-only measurement report for one run (schema_version 1).
//! The report is a private measurement artifact for the person and later offline analysis, never a prompt:
//! nothing reads it back into a model-facing message. It is derived only from the trace records a run already
//! wrote (trace.jsonl) plus the run's own summary, so writing it cannot change what the run measured. Groups:
//! identifiers; known usage totals with explicit attribution gaps; active
//! context (provider-reported estimates only — lifetime consumption is never substituted for it); amplification;
//! outcome; coverage. Missing counters and unresolved aggregate overlap are reported explicitly. The differential
//! gates (`tests/gate_taskreport.rs`, `tests/gate_harness.rs`) compare this against the Python module.

use crate::rt::harness::py_sum;
use crate::rt::py::{self, Num};
use crate::rt::{Error, Exc, Result};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 1;
pub const TOKEN_KEYS: [&str; 4] = ["input", "cache_write", "cache_read", "output"];
const PAID: [&str; 3] = ["ok", "malformed", "error"]; // settled attempts included in the harness's usage ledger
const FAILED: [&str; 2] = ["error", "fatal"];
const ACCEPTED: [&str; 2] = ["done", "ready_for_review"]; // final states that mean the task's output was accepted

const NORMALIZATION_NOTE: &str = "'input' is uncached input: the capture paths already separate cached tokens (the Codex path subtracts cached_input_tokens from the provider's total input; the Claude path's input_tokens excludes the cache fields by the provider's own accounting), so cache-write and cache-read are never added into it.";
const SCOPE_NOTE: &str = "Each distinct backend call record is counted once. A 'models' usage field marks a CLI-turn aggregate; its model subtotals are not added again. Backend call records can cover multiple provider requests. Separately recorded parent/child overlap cannot be resolved without linkage.";
const RAW_FIELDS_NOTE: &str = "requests[].raw_usage preserves the normalized usage dict captured by the backend, not the original provider wire event; warning text is replaced with warning_count.";

type M = Map<String, Value>;
type Operand = std::result::Result<Num, &'static str>;

/// value when it is a non-negative integer (a bool is not a count), else None.
fn nonneg(value: &Value) -> bool {
    nonneg_int(value).is_some()
}

fn nonneg_int(v: &Value) -> Option<i128> {
    py::int_of(v).filter(|n| *n >= 0)
}

fn started_num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) if n.is_i64() => Some(n.as_i64().unwrap() as f64),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn gap(kind: &str, note: &str) -> Value {
    let mut m = M::new();
    m.insert("kind".into(), kind.into());
    m.insert("note".into(), note.into());
    Value::Object(m)
}

fn obj(v: &mut Value) -> &mut M {
    v.as_object_mut().expect("a gap is an object")
}

fn insert(v: &mut Value, k: &str, x: Value) {
    let m = obj(v);
    let note = m.shift_remove("note");
    m.insert(k.into(), x);
    if let Some(note) = note { m.insert("note".into(), note); }
}

fn string_value(v: Option<String>) -> Value {
    v.map(Value::String).unwrap_or(Value::Null)
}

fn pyjson_float(f: f64) -> Value {
    crate::rt::pyjson::float_value(f)
}

/// Python's `==` across numbers: `1.0 == 1` is true.
fn num_eq(a: &Value, b: &Value) -> bool {
    match (Num::from_value(a), Num::from_value(b)) {
        (Some(x), Some(y)) => x.py_cmp(y).map(|o| o.is_eq()).unwrap_or(false),
        (None, None) => a == b,
        _ => false,
    }
}

/// Trace records in file order with exact duplicates collapsed, and the ordering problems named. A duplicated
/// event line must not be counted twice; consumption itself is order-independent.
fn dedupe(records: &[Value], gaps: &mut Vec<Value>) -> Vec<Value> {
    let mut kept: Vec<Value> = vec![];
    let mut seen: HashSet<String> = HashSet::new(); // fixed-size digests, not whole serialized records
    let mut duplicates = 0;
    let mut started: Vec<f64> = vec![];
    for rec in records {
        if !rec.is_object() {
            gaps.push(gap("unparseable_trace_line", "a trace line that is not a JSON object was skipped"));
            continue;
        }
        let mut opts = crate::rt::pyjson::Opts::default();
        opts.sort_keys = true;
        let identity = crate::rt::sha256_hex(crate::rt::pyjson::dumps_with(rec, opts).as_bytes());
        if !seen.insert(identity) {
            duplicates += 1;
            continue;
        }
        if let Some(v) = rec.get("started").and_then(started_num) {
            started.push(v);
        }
        kept.push(rec.clone());
    }
    if duplicates > 0 {
        let mut g = gap("duplicate_records", "identical trace lines were collapsed so a duplicated event is not counted twice");
        insert(&mut g, "dropped", Value::from(duplicates));
        gaps.push(g);
    }
    if started.windows(2).any(|w| w[1] < w[0]) {
        gaps.push(gap("out_of_order_records", "records carry out-of-order timestamps; consumption is order-independent"));
    }
    kept
}

/// ({category: count or None}, problems) for one usage dict: an absent or invalid count stays unknown.
fn counters(usage: &M) -> (M, Vec<String>) {
    let mut categories = M::new();
    let mut problems = vec![];
    for key in TOKEN_KEYS {
        match usage.get(key) {
            Some(v) if nonneg(v) => {
                categories.insert(key.into(), v.clone());
            }
            Some(v) => {
                categories.insert(key.into(), Value::Null);
                if !v.is_null() {
                    problems.push(format!("{key} is not a non-negative integer"));
                }
            }
            None => {
                categories.insert(key.into(), Value::Null);
            }
        }
    }
    (categories, problems)
}

/// The usage dict as captured, with a warning-text list bounded to a count (the report must stay bounded).
fn raw_usage(usage: &M) -> Value {
    let mut raw: M = usage.iter().filter(|(k, _)| *k != "warnings").map(|(k, v)| (k.clone(), v.clone())).collect();
    if let Some(Value::Array(w)) = usage.get("warnings") {
        raw.insert("warning_count".into(), Value::from(w.len()));
    }
    Value::Object(raw)
}

/// One row per kept record that carries a usage dict, with normalized counters, raw usage and scope.
fn rows_of(kept: &[Value], gaps: &mut Vec<Value>) -> Vec<Value> {
    let mut rows = vec![];
    for rec in kept {
        let Some(usage) = rec.get("usage").and_then(Value::as_object) else { continue };
        let (counters, problems) = counters(usage);
        let mut row = M::new();
        for k in ["step", "attempt", "status", "backend", "model", "started"] {
            row.insert(k.into(), rec.get(k).cloned().unwrap_or(Value::Null));
        }
        if !rec.get("status").is_some_and(Value::is_string) {
            row.insert("status".into(), "unknown".into());
            gaps.push(gap("invalid_status", "non-string call status grouped as unknown"));
        }
        row.insert("counters".into(), Value::Object(counters.clone()));
        row.insert("raw_usage".into(), raw_usage(usage));
        row.insert(
            "scope".into(),
            if usage.contains_key("models") { "provider_turn_aggregate".into() } else { "backend_call".into() },
        );
        let settled = matches!(rec.get("status"), Some(Value::String(s)) if PAID.contains(&s.as_str()));
        if rec.get("stopped").map(py::truthy).unwrap_or(false) {
            row.insert("stopped".into(), rec["stopped"].clone());
        }
        if !settled {
            row.insert("unsettled".into(), Value::Bool(true)); // its counts are kept but never trusted into a ledger
        }
        rows.push(Value::Object(row));
        for problem in problems {
            let mut g = gap("invalid_counter", "");
            let o = obj(&mut g);
            o.shift_remove("note");
            o.insert("step".into(), rec.get("step").cloned().unwrap_or(Value::Null));
            o.insert("attempt".into(), rec.get("attempt").cloned().unwrap_or(Value::Null));
            o.insert("detail".into(), problem.into());
            gaps.push(g);
        }
        if counters.values().any(|v| !v.is_null()) && !settled {
            let mut g = gap(
                "usage_on_unsettled_record",
                "token counts on a record that never settled as paid are kept as raw fields but excluded from every ledger",
            );
            let o = obj(&mut g);
            let note = o.shift_remove("note").unwrap();
            o.insert("step".into(), rec.get("step").cloned().unwrap_or(Value::Null));
            o.insert("attempt".into(), rec.get("attempt").cloned().unwrap_or(Value::Null));
            o.insert("status".into(), rec.get("status").cloned().unwrap_or(Value::Null));
            o.insert("note".into(), note);
            gaps.push(g);
        }
    }
    discontinuities(&rows, gaps);
    rows
}

/// Raw fields that look cumulative must never fall between the requests of one task: a fall means the provider
/// restarted a counter (or the capture mixed cumulative and incremental values), so it is flagged instead of
/// being summed silently.
fn discontinuities(rows: &[Value], gaps: &mut Vec<Value>) {
    let mut last: HashMap<String, i128> = HashMap::new();
    for row in rows {
        let o = row.as_object().expect("a row is an object");
        let Some(raw) = o.get("raw_usage").and_then(Value::as_object) else { continue };
        for (field, value) in raw {
            let lower = field.to_lowercase();
            if !(lower.contains("total") || lower.contains("cumulative")) {
                continue;
            }
            let Some(value) = nonneg_int(value) else { continue };
            let key = format!("{}\u{0}{}", o.get("step").map(py::value_str).unwrap_or_default(), field);
            if let Some(previous) = last.get(&key) {
                if value < *previous {
                    let mut g = gap(
                        "counter_discontinuity",
                        "a cumulative-style counter fell between requests; the ledger sums the normalized per-request counts, never the raw cumulative fields",
                    );
                    let go = obj(&mut g);
                    let note = go.shift_remove("note").unwrap();
                    go.insert("step".into(), o.get("step").cloned().unwrap_or(Value::Null));
                    go.insert("attempt".into(), o.get("attempt").cloned().unwrap_or(Value::Null));
                    go.insert("field".into(), field.clone().into());
                    go.insert("from".into(), Num::Int(*previous).to_value());
                    go.insert("to".into(), Num::Int(value).to_value());
                    go.insert("note".into(), note);
                    gaps.push(g);
                }
            }
            let high = value.max(*last.get(&key).unwrap_or(&value));
            last.insert(key, high);
        }
    }
}

/// ({category: sum}, requests, unknown_inputs) over rows: unknown counts are skipped by the sums and named,
/// never read as zero.
fn ledger(rows: &[Value]) -> Result<(Value, Value, i64)> {
    let mut sums: Vec<Num> = vec![Num::Int(0); TOKEN_KEYS.len()];
    let mut requests = 0i64;
    let mut unknown_inputs = 0i64;
    for row in rows {
        let o = row.as_object().expect("a row is an object");
        if o.get("unsettled").map(py::truthy).unwrap_or(false) {
            continue;
        }
        let counters = o.get("counters").and_then(Value::as_object).expect("a row carries counters");
        let known: Vec<bool> = TOKEN_KEYS.iter().map(|k| !counters[*k].is_null()).collect();
        if known.iter().any(|b| *b) {
            requests += 1;
            for (i, k) in TOKEN_KEYS.iter().enumerate() {
                let v = if counters[*k].is_null() { Value::from(0) } else { counters[*k].clone() };
                let x = Num::from_value(&v).ok_or_else(py::too_big)?;
                sums[i] = num_add(sums[i].clone(), x, "+=")?;
            }
            if known.iter().any(|b| !*b) {
                unknown_inputs += 1;
            }
        }
    }
    let mut out = M::new();
    for (k, n) in TOKEN_KEYS.iter().zip(&sums) {
        out.insert((*k).into(), n.to_value());
    }
    out.insert("total".into(), sum_num(&sums)?.to_value());
    out.insert("requests".into(), Value::from(requests));
    Ok((Value::Object(out), Value::from(requests), unknown_inputs))
}

fn sum_num(nums: &[Num]) -> Result<Num> {
    let operands: Vec<Operand> = nums.iter().cloned().map(Ok).collect();
    py_sum(&operands)
}

fn num_add(acc: Num, x: Num, op: &str) -> Result<Num> {
    match (acc, x) {
        (Num::Int(a), Num::Int(b)) => a.checked_add(b).map(Num::Int).ok_or_else(py::too_big),
        (a, b) => {
            let _ = op;
            Ok(Num::Float(a.as_f64() + b.as_f64()))
        }
    }
}

/// Python's dict(rec); dict(rec).update(usage): the usage's value wins, the first position stays.
fn candidates(rec: &Value) -> M {
    let mut m = M::new();
    if let Some(o) = rec.as_object() {
        for (k, v) in o {
            m.insert(k.clone(), v.clone());
        }
    }
    if let Some(u) = rec.get("usage").and_then(Value::as_object) {
        for (k, v) in u {
            m.insert(k.clone(), v.clone());
        }
    }
    m
}

/// Provider-reported active-context estimates, if any record carries one. Lifetime consumption is never
/// substituted for active context.
fn active_context(kept: &[Value]) -> Value {
    let mut estimates: Vec<Value> = vec![];
    for rec in kept {
        for (key, value) in candidates(rec) {
            if ["context_tokens", "active_context_tokens", "context_size_tokens"].contains(&key.to_lowercase().as_str()) && nonneg(&value) {
                let mut e = M::new();
                e.insert("step".into(), rec.get("step").cloned().unwrap_or(Value::Null));
                e.insert("attempt".into(), rec.get("attempt").cloned().unwrap_or(Value::Null));
                e.insert("source".into(), key.into());
                e.insert("value".into(), value);
                e.insert("started".into(), rec.get("started").cloned().unwrap_or(Value::Null));
                estimates.push(Value::Object(e));
            }
        }
    }
    if estimates.is_empty() {
        let mut m = M::new();
        m.insert("available".into(), Value::Bool(false));
        m.insert("note".into(), "no record in this trace carries a provider-reported active-context estimate; lifetime consumption is not active context (it counts every token the task ever spent, not what the next call would load).".into());
        return Value::Object(m);
    }
    let values: Vec<i128> = estimates.iter().filter_map(|e| nonneg_int(&e["value"])).collect();
    let latest = estimates.last().expect("non-empty").clone();
    let mut m = M::new();
    m.insert("available".into(), Value::Bool(true));
    m.insert("latest".into(), latest["value"].clone());
    m.insert("source".into(), latest["source"].clone());
    m.insert("peak".into(), Num::Int(values.iter().max().copied().unwrap_or(0)).to_value());
    m.insert("trajectory".into(), Value::Array(estimates));
    m.insert("uncertainty".into(), "provider estimates are approximate: they are sampled at different moments of each call and can exclude prompt parts the provider does not report".into());
    Value::Object(m)
}

fn compactions(kept: &[Value], requests: &Value) -> Result<Value> {
    let events: Vec<&Value> = kept
        .iter()
        .filter(|rec| {
            candidates(rec).iter().any(|(k, v)|
                ["compacted", "compaction_count", "context_compacted"].contains(&k.to_lowercase().as_str())
                && (*v == Value::Bool(true) || nonneg_int(v).is_some_and(|n| n > 0)))
        })
        .collect();
    if events.is_empty() {
        let mut m = M::new();
        m.insert("count".into(), Value::from(0));
        m.insert("requests_between".into(), Value::Null);
        m.insert("note".into(), "no record in this trace marks a compaction; 0 means none was observed, not that none happened inside the provider".into());
        return Ok(Value::Object(m));
    }
    // `round(requests / len(events), 1) if requests else None`: a non-numeric request count is Python's TypeError.
    let count = if py::truthy(requests) {
        let n = Num::from_value(requests).ok_or_else(|| {
            Error::new(Exc::Type, format!("unsupported operand type(s) for /: 'int' and '{}'", py::type_name(requests)))
        })?;
        pyjson_float(py::round(n.as_f64() / events.len() as f64, 1))
    } else {
        Value::Null
    };
    let mut m = M::new();
    m.insert("count".into(), Value::from(events.len()));
    m.insert("requests_between".into(), count);
    m.insert("note".into(), "compaction records found in the trace".into());
    Ok(Value::Object(m))
}

/// (bytes, count) of the trace's steps/*.raw.txt artifacts, or (None, 0) without a trace: these files hold bytes
/// of raw backend output. Artifact bytes are measured separately from token counters.
fn tool_artifacts(trace: Option<&Path>) -> (Value, i64) {
    let Some(trace) = trace else { return (Value::Null, 0) };
    let folder = py::join(trace, "steps");
    if !folder.is_dir() {
        return (Value::from(0), 0);
    }
    let Ok(entries) = std::fs::read_dir(&folder) else { return (Value::Null, 0) };
    let mut names: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.to_string_lossy().ends_with(".raw.txt")).collect();
    names.sort();
    let mut total = 0i64;
    for p in &names {
        match p.metadata() {
            Ok(m) => total += m.len() as i64,
            Err(_) => return (Value::Null, 0),
        }
    }
    (Value::from(total), names.len() as i64)
}

fn json_obj(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut m = M::new();
    for (k, v) in entries {
        m.insert(k.into(), v);
    }
    Value::Object(m)
}

/// `socket.gethostname()`: the machine name, or None when it cannot be read (or is empty).
pub fn hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc != 0 {
            return None;
        }
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]).into_owned();
        if name.is_empty() { None } else { Some(name) }
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty())
    }
}

/// `getpass.getuser()`: the login name from the environment, or None when unset.
pub fn getuser() -> Option<String> {
    for key in ["LOGNAME", "USER", "LNAME", "USERNAME"] {
        if let Ok(name) = std::env::var(key) {
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// The report dict for these trace records. Pure over its inputs apart from stat-ing the trace's raw artifacts
/// when trace is given: same records in, same report out; nothing is written.
#[allow(clippy::too_many_arguments)]
pub fn build(
    records: &[Value],
    summary: Option<&M>,
    blockers: &[Value],
    final_state: &str,
    trace: Option<&Path>,
    max_tokens: Option<Num>,
    overshoot_calls: Option<i64>,
    cost_usd: Option<f64>,
    task_id: Option<String>,
    started: Option<f64>,
) -> Result<Value> {
    let empty = M::new();
    let summary = summary.unwrap_or(&empty);
    let mut gaps: Vec<Value> = vec![];
    let mut exclusions: Vec<Value> = vec![];
    let mut notes: Vec<Value> = vec![];
    let kept = dedupe(records, &mut gaps);
    let rows = rows_of(&kept, &mut gaps);
    let (mut ledger_v, _, unknown_inputs) = ledger(&rows)?;
    if rows.is_empty() && summary_truthy_calls(summary)? {
        gaps.push(gap(
            "no_trace_records",
            "no trace records were available; per-request detail is unknown and the sums below come only from the run summary",
        ));
        if TOKEN_KEYS.iter().any(|k| summary.get(*k).is_some_and(py::truthy)) {
            let mut m = M::new();
            for k in TOKEN_KEYS {
                let v = summary.get(k).cloned().unwrap_or(Value::Null);
                m.insert(k.into(), if py::truthy(&v) { v } else { Value::from(0) });
            }
            let operands = TOKEN_KEYS.iter().map(|k| operand(&m[*k])).collect::<Result<Vec<_>>>()?;
            let total = py_sum(&operands)?;
            m.insert("total".into(), total.to_value());
            m.insert("requests".into(), Value::Null);
            ledger_v = Value::Object(m);
        }
    }
    let mut by_status: Vec<(Value, Vec<Value>)> = vec![];
    for row in &rows {
        let status = &row["status"];
        if status.is_array() || status.is_object() {
            let t = py::type_name(status);
            return Err(Error::new(Exc::Type, format!("cannot use '{t}' as a dict key (unhashable type: '{t}')")));
        }
        match by_status.iter_mut().find(|(s, _)| py::eq(s, status)) {
            Some((_, group)) => group.push(row.clone()),
            None => by_status.push((status.clone(), vec![row.clone()])),
        }
    }
    // Compare in insertion order before sorting, as Python does when discovering its first run.
    for pair in by_status.windows(2) { status_cmp(&pair[1].0, &pair[0].0)?; }
    by_status.sort_by(|a, b| status_cmp(&a.0, &b.0).expect("status types were checked"));
    let mut statuses = M::new();
    for (status, group) in &by_status {
        let name = match status {
            Value::String(s) => s.clone(),
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            other => py::value_str(other),
        };
        let (ledger, _, _) = ledger(group)?;
        statuses.insert(name, ledger);
    }
    let paid_rows = kept.iter().filter(|r| matches!(r.get("status"), Some(Value::String(s)) if PAID.contains(&s.as_str()))).count() as i64;

    let has_tokens = |rec: &Value| -> bool {
        rec.get("usage")
            .and_then(Value::as_object)
            .map(|u| TOKEN_KEYS.iter().any(|k| u.get(*k).map(nonneg).unwrap_or(false)))
            .unwrap_or(false)
    };
    // a paid record whose usage carries no token fields — or none at all — is an unknown, never a zero request
    let unknown_requests = kept
        .iter()
        .filter(|rec| matches!(rec.get("status"), Some(Value::String(s)) if PAID.contains(&s.as_str())) && !has_tokens(rec))
        .count() as i64;
    // the harness counts a paid call as untokened when any category is missing, so reconcile against that count
    let incomplete_requests = kept
        .iter()
        .filter(|rec| matches!(rec.get("status"), Some(Value::String(s)) if PAID.contains(&s.as_str())))
        .filter(|rec| !rec.get("usage").and_then(Value::as_object).is_some_and(|u| TOKEN_KEYS.iter().all(|k| u.get(*k).is_some_and(nonneg))))
        .count() as i64;
    let untokened_summary = summary.get("untokened_calls").cloned().unwrap_or(Value::Null);
    if !rows.is_empty() && !untokened_summary.is_null() && !num_eq(&untokened_summary, &Value::from(incomplete_requests)) {
        let mut g = gap("untokened_mismatch", "the run summary and the trace disagree about calls that reported no token counts");
        insert(&mut g, "records", Value::from(incomplete_requests));
        insert(&mut g, "summary", untokened_summary.clone());
        gaps.push(g);
    }
    if unknown_inputs > 0 {
        let mut g = gap(
            "unknown_counters",
            "these requests reported some categories but not others; the sums skip the unknown categories, so they understate those requests",
        );
        insert(&mut g, "requests", Value::from(unknown_inputs));
        gaps.push(g);
    }
    let whole = ledger_v.as_object().expect("the ledger is an object").clone();
    let uncached = whole.get("input").cloned().unwrap_or(Value::from(0));
    let requests_v = whole.get("requests").cloned().unwrap_or(Value::Null);
    let (artifact_bytes, artifact_count) = tool_artifacts(trace);
    let mut consumed = M::new();
    consumed.insert("whole_task".into(), ledger_v.clone());
    consumed.insert("by_status".into(), Value::Object(statuses.clone()));
    consumed.insert("requests".into(), Value::Array(rows.clone()));
    let mut normalization = M::new();
    normalization.insert("note".into(), NORMALIZATION_NOTE.into());
    normalization.insert("raw_fields_kept".into(), RAW_FIELDS_NOTE.into());
    normalization.insert("accounting_scopes".into(), SCOPE_NOTE.into());
    consumed.insert("normalization".into(), Value::Object(normalization));
    consumed.insert("unknown_usage_requests".into(), Value::from(unknown_requests));
    consumed.insert(
        "untokened_calls".into(),
        if summary.contains_key("untokened_calls") { untokened_summary.clone() } else { Value::from(incomplete_requests) },
    );
    if rows.iter().any(|r| r["scope"] == "provider_turn_aggregate") {
        gaps.push(gap("aggregate_attribution_unknown", "aggregate model subtotals are not added again; separately recorded child overlap and the underlying provider request count are unavailable"));
    }
    let mut signatures: Vec<Value> = vec![];
    let mut repeat_events = vec![];
    for blocker in blockers {
        if !blocker.is_object() {
            return Err(Error::new(Exc::Attribute, format!("'{}' object has no attribute 'get'", py::type_name(blocker))));
        }
        let signature = blocker.get("signature").cloned().unwrap_or(Value::Null);
        if signature.is_array() || signature.is_object() {
            let t = py::type_name(&signature);
            return Err(Error::new(Exc::Type, format!("cannot use '{t}' as a set element (unhashable type: '{t}')")));
        }
        if !signatures.iter().any(|s| py::eq(s, &signature)) { signatures.push(signature); }
        let count = blocker.get("count").cloned().unwrap_or(Value::from(1));
        let count = match count {
            Value::Bool(b) => Num::Int(b as i128),
            Value::Number(_) => Num::from_value(&count).ok_or_else(py::too_big)?,
            _ => return Err(Error::new(Exc::Type, format!("unsupported operand type(s) for -: '{}' and 'int'", py::type_name(&count)))),
        };
        let repeats = match count {
            Num::Int(n) => Num::Int(n.checked_sub(1).ok_or_else(py::too_big)?),
            Num::Float(n) => Num::Float(n - 1.0),
        };
        repeat_events.push(if matches!(repeats.py_cmp(Num::Int(0)), Some(std::cmp::Ordering::Greater)) { repeats } else { Num::Int(0) });
    }
    let mut repeated = M::new();
    repeated.insert("signatures".into(), Value::from(signatures.len()));
    repeated.insert("repeat_events".into(), sum_num(&repeat_events)?.to_value());
    repeated.insert("note".into(), "signatures hash failure output only; identical output does not establish unchanged files or repeated file reads".into());
    let accepted = ACCEPTED.contains(&final_state);
    // `round(uncached / whole['requests'], 1) if whole.get('requests') else None`
    let uncached_input_per_usage_record = if py::truthy(&requests_v) {
        let n = Num::from_value(&requests_v).ok_or_else(|| {
            Error::new(Exc::Type, format!("unsupported operand type(s) for /: 'int' and '{}'", py::type_name(&requests_v)))
        })?;
        let input = Num::from_value(&uncached).ok_or_else(py::too_big)?;
        pyjson_float(py::round(input.as_f64() / n.as_f64(), 1))
    } else {
        Value::Null
    };
    let mut amplification = M::new();
    amplification.insert("uncached_input_per_usage_record".into(), uncached_input_per_usage_record);
    amplification.insert("uncached_input_at_completed_state".into(), if accepted { uncached } else { Value::Null });
    amplification.insert("raw_artifact_bytes".into(), artifact_bytes);
    amplification.insert("raw_artifacts".into(), Value::from(artifact_count));
    amplification.insert("repeated_failure_outputs".into(), Value::Object(repeated));
    amplification.insert("blockers".into(), Value::Array(blockers.to_vec()));
    amplification.insert("compactions".into(), compactions(&kept, &requests_v)?);
    amplification.insert(
        "notes".into(),
        Value::Array(vec![
            "uncached_input_at_completed_state is null unless the final state is an accepted outcome".into(),
            "raw_artifact_bytes counts captured backend output, not all tool results; bytes are outside every token count".into(),
        ]),
    );
    let stopped = kept
        .iter()
        .filter(|r| r.as_object().map(|o| o.get("stopped").map(py::truthy).unwrap_or(false)).unwrap_or(false))
        .count() as i64;
    let calls_value = summary.get("calls").cloned().unwrap_or(Value::Null);
    let calls_completed = if !calls_value.is_null() {
        let unfinished = summary.get("unfinished_calls").cloned().unwrap_or(Value::from(0));
        let a = Num::from_value(&calls_value).ok_or_else(|| {
            Error::new(Exc::Type, format!("unsupported operand type(s) for -: '{}' and 'int'", py::type_name(&calls_value)))
        })?;
        let b = Num::from_value(&unfinished).ok_or_else(|| {
            Error::new(Exc::Type, format!("unsupported operand type(s) for -: 'int' and '{}'", py::type_name(&unfinished)))
        })?;
        match (a, b) {
            (Num::Int(a), Num::Int(b)) => Value::from(a.checked_sub(b).ok_or_else(py::too_big)?),
            (a, b) => pyjson_float(a.as_f64() - b.as_f64()),
        }
    } else {
        Value::from(paid_rows)
    };
    let mut outcome = M::new();
    outcome.insert("final_state".into(), final_state.into());
    outcome.insert("calls_completed".into(), calls_completed);
    outcome.insert(
        "calls_failed".into(),
        Value::from(kept.iter().filter(|r| matches!(r["status"], Value::String(ref s) if FAILED.contains(&s.as_str()))).count() as i64),
    );
    outcome.insert("stopped_calls".into(), Value::from(stopped));
    outcome.insert("elapsed_seconds".into(), summary.get("elapsed_s").cloned().unwrap_or(Value::Null));
    outcome.insert("cost_usd".into(), cost_usd.map(pyjson_float).unwrap_or(Value::Null));
    outcome.insert("unpriced_calls".into(), summary.get("unpriced_calls").cloned().unwrap_or(Value::Null));
    outcome.insert("admission_refusals".into(), summary.get("admission_refusals").cloned().unwrap_or(Value::Null));
    outcome.insert("incidents".into(), summary.get("incidents").cloned().unwrap_or(Value::Null));
    let mut outcome_notes = vec![
        "cost_usd is the accounted cost: provider-reported prices plus any configured unpriced-call fallback charges; subscription token usage is not a monetary invoice".to_string(),
        "acceptance checks and constraint retention are not recorded by this harness and stay unknown".to_string(),
        "a budget stop is an incomplete outcome; the caller decides the final state".to_string(),
    ];
    if cost_usd.is_none() && summary.get("unpriced_calls").is_some_and(py::truthy) {
        outcome_notes.push("cost_usd is null because some calls reported no price, so the total is not supportable".to_string());
    }
    outcome.insert("notes".into(), Value::Array(outcome_notes.into_iter().map(Value::String).collect()));
    if let Some(max_tokens) = &max_tokens {
        // Python appends this key after 'notes' exists, so it sorts after it in the serialized object.
        let mut budget = M::new();
        budget.insert("max_tokens".into(), max_tokens.to_value());
        let recorded: Vec<Value> = TOKEN_KEYS
            .iter()
            .map(|k| {
                let v = summary.get(*k).cloned().unwrap_or(Value::Null);
                if py::truthy(&v) { v } else { Value::from(0) }
            })
            .collect();
        let operands: Vec<Operand> = recorded
            .iter()
            .map(operand)
            .collect::<Result<_>>()?;
        budget.insert("recorded_tokens".into(), py_sum(&operands)?.to_value());
        budget.insert("overshoot".into(), overshoot_calls.map(Value::from).unwrap_or(Value::Null));
        outcome.insert("budget".into(), Value::Object(budget));
    }
    exclusions.push("the task report is a private artifact and is never sent to a model".into());
    exclusions.push("tool-result artifacts hold bytes, not tokens; they are outside every token count".into());
    exclusions.push("subscription token usage is not a monetary invoice".into());
    notes.push("ledger totals sum known counters only; missing categories are coverage gaps, not measured zero".into());
    notes.push("identifiers for experiment, job, thread and parent are not recorded by this harness and stay unknown".into());
    notes.push("provider-reported usage is the only source for every token category here".into());
    let mut identifiers = M::new();
    identifiers.insert("task_id".into(), string_value(task_id));
    identifiers.insert("started".into(), started.map(pyjson_float).unwrap_or(Value::Null));
    identifiers.insert("hostname".into(), string_value(hostname()));
    identifiers.insert("user".into(), string_value(getuser()));
    let mut coverage = M::new();
    coverage.insert("gaps".into(), Value::Array(gaps));
    coverage.insert("exclusions".into(), Value::Array(exclusions));
    coverage.insert("notes".into(), Value::Array(notes));
    Ok(json_obj([
        ("schema_version", Value::from(SCHEMA_VERSION)),
        ("identifiers", Value::Object(identifiers)),
        ("consumption", Value::Object(consumed)),
        ("active_context", active_context(&kept)),
        ("amplification", Value::Object(amplification)),
        ("outcome", Value::Object(outcome)),
        ("coverage", Value::Object(coverage)),
    ]))
}

fn status_cmp(a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
    if let (Value::String(a), Value::String(b)) = (a, b) { return Ok(a.cmp(b)); }
    let numeric = |v: &Value| match v { Value::Bool(b) => Some(Num::Int(*b as i128)), _ => Num::from_value(v) };
    if let (Some(a), Some(b)) = (numeric(a), numeric(b)) { return Ok(a.py_cmp(b).unwrap_or(std::cmp::Ordering::Equal)); }
    Err(Error::new(Exc::Type, format!("'<' not supported between instances of '{}' and '{}'", py::type_name(a), py::type_name(b))))
}

fn operand(v: &Value) -> Result<Operand> {
    match v {
        Value::Bool(b) => Ok(Ok(Num::Int(*b as i128))),
        Value::Number(_) => Num::from_value(v).map(Ok).ok_or_else(py::too_big),
        _ => Ok(Err(py::type_name(v))),
    }
}

/// `(summary.get('calls') or 0) > 0`: a non-number that is truthy is Python's TypeError.
fn summary_truthy_calls(summary: &M) -> Result<bool> {
    let calls = summary.get("calls").cloned().unwrap_or(Value::Null);
    if !py::truthy(&calls) {
        return Ok(false);
    }
    let n = match calls { Value::Bool(b) => Num::Int(b as i128), _ => Num::from_value(&calls).ok_or_else(||
        Error::new(Exc::Type, format!("'>' not supported between instances of '{}' and 'int'", py::type_name(&calls))))? };
    Ok(matches!(n.py_cmp(Num::Int(0)), Some(std::cmp::Ordering::Greater)))
}
