//! Differential gate for `rt::workflow`: each ported function against workflow.py on the same inputs, offline,
//! compared byte for byte. Each test sends one batch of cases to one python3 process. Corpus sweeps are
//! `#[ignore]`d: run them with `cargo test --test gate_workflow -- --ignored`.
#![cfg(unix)]

use ringo_cli::rt::workflow::{self as wf, difflib};
use ringo_cli::rt::harness::{Backend, CallFn};
use ringo_cli::rt::py::Num;
use ringo_cli::rt::{pyjson, Error};
use serde_json::{json, Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DRIVER: &str = r#"
import json, struct, sys, difflib
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import workflow, harness


def surrogate_free(v):
    try:
        json.dumps(v, ensure_ascii=False).encode('utf-8')
        return True
    except UnicodeEncodeError:
        return False


def strict_json(text):
    v = workflow.strict_json(text)
    return v if surrogate_free(v) else {'__surrogate__': True}


def strict_kind(text):
    workflow.strict_json(text)
    return 'ok'


def json_boundary(open_, inner, close):
    def deep(n):
        try:
            workflow.strict_json(open_ * n + inner + close * n)
        except ValueError as e:
            return 'nesting' in str(e)
        return False
    lo, hi = 1, 400000
    while lo < hi:
        mid = (lo + hi + 1) // 2
        if deep(mid):
            hi = mid - 1
        else:
            lo = mid
    return lo


def text_pipeline(current, declared, text):
    reply = workflow.parse_text_reply(text)
    if not surrogate_free(reply):
        return {'__surrogate__': True}
    try:
        return {'reply': reply, 'files': workflow.materialize_text(current, declared, reply)}
    except BaseException as e:
        return {'reply': reply, 'error': [type(e).__name__, str(e)]}


def json_protocol(current, declared, text):
    candidate = workflow.strict_json(text)
    files = workflow.materialize(current, declared, candidate)
    return [candidate, files] if surrogate_free(candidate) else {'__surrogate__': True}


def matcher(a, b, autojunk):
    m = difflib.SequenceMatcher(None, a, b, autojunk=autojunk)
    return [[list(x) for x in m.get_matching_blocks()], [list(x) for x in m.get_opcodes()],
            [[list(x) for x in g] for g in m.get_grouped_opcodes(3)]]


def listing(root):
    root = Path(root)
    return sorted(p.relative_to(root).as_posix() + ':' + (p.read_text(encoding='utf-8') if p.is_file() else '/')
                  for p in root.rglob('*'))


def write_files(root, files):
    workflow.write_files(Path(root), files)
    return listing(root)


def write_json(path, value):
    workflow.write_json(Path(path), value)
    return Path(path).read_text(encoding='utf-8')


def checks_case(spec, root, logdir, remaining):
    rows = workflow.checks(spec, Path(root), Path(logdir), lambda: remaining)
    for r in rows:
        r['seconds'] = type(r['seconds']).__name__
    return rows


ENV_KEYS = ('COLUMNS', 'LINES', 'NO_COLOR', 'FORCE_COLOR', 'PYTHON_COLORS', 'TERM')


def main_case(argv, env):
    import contextlib, io, os
    saved = {k: os.environ.get(k) for k in ENV_KEYS}
    for k in ENV_KEYS:
        os.environ.pop(k, None)
    os.environ.update(env)
    out, err = io.StringIO(), io.StringIO()
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = workflow.main(argv)
            except SystemExit as e:
                code = e.code
    finally:
        for k, v in saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
    return [code, out.getvalue(), err.getvalue()]


def make_error(kind, msg, data):
    import subprocess, backends
    if kind == 'AdmissionRefused':
        return harness.AdmissionRefused(msg)
    if kind == 'BudgetExceeded':
        return harness.BudgetExceeded(msg)
    if kind == 'CallError':
        return harness.CallError(msg, data.get('usage'), data.get('raw'))
    if kind == 'JudgeError':
        return backends.JudgeError(msg)
    if kind == 'Stopped':
        return backends.Stopped(['x'], 1.0)
    if kind == 'TimeoutExpired':
        return subprocess.TimeoutExpired(['x'], 1.0)
    if kind == 'KeyError':
        return KeyError(msg)
    return ValueError(msg)


def run_case(spec_path, out, replies, mode):
    import time
    seq, seen = iter(replies), []

    def body(backend, system, message, timeout_s, watch=None):
        seen.append([backend, harness.sha(system), message])
        if message == harness.PROBE_MESSAGE:
            return 'OK', {'input': 1, 'output': 1, 'cache_read': 0, 'cache_write': 0}
        item = next(seq)
        if item.get('soft') and watch is not None:
            # the soft limit passed and the call ended while its probe ran
            watch.done.set()
            watch.soft(1.5, lambda: ('{"type": "turn.started"}\nnot json\n', 'stream error: reconnecting\n'))
        if item.get('sleep'):
            time.sleep(item['sleep'])
        if 'raise' in item:
            raise make_error(*item['raise'])
        return item['reply'], item.get('usage', {})

    if mode == 'watch':
        def call(backend, system, message, timeout_s, watch=None):
            return body(backend, system, message, timeout_s, watch)
    else:
        def call(backend, system, message, timeout_s):
            return body(backend, system, message, timeout_s)
    try:
        result = {'report': workflow.run(spec_path, out, call_fn=call)}
    except BaseException as e:
        result = {'raised': [type(e).__name__, str(e)]}
    result['seen'] = seen
    return result


def identity():
    import releases
    return releases.identity(workflow.HERE)


H = {
    'checks': checks_case, 'bridge': workflow.bridge, 'outcome': workflow.outcome, 'main': main_case,
    'run': run_case, 'identity': identity, 'review_findings': workflow.review_findings,
    'strict_json': strict_json, 'strict_kind': strict_kind, 'json_boundary': json_boundary, 'file_map': workflow.file_map,
    'near_miss': workflow.near_miss, 'materialize': workflow.materialize, 'json_protocol': json_protocol,
    'parse_text_reply': workflow.parse_text_reply, 'text_pipeline': text_pipeline,
    'materialize_text': workflow.materialize_text, 'artifact_kind': workflow.artifact_kind,
    'numbers': lambda text: [[str(x), struct.unpack('<Q', struct.pack('<d', float(x)))[0]]
                             for x in sorted(workflow.numbers(text))],
    'constants': lambda: [workflow.LIMIT, workflow.MARK, workflow.REPAIR, list(workflow.ROLES), list(workflow.EFFORTS),
                          list(workflow.DOC_SUFFIXES), workflow.NUMBER.pattern],
    'text_of': workflow.text_of, 'text_message': workflow.text_message, 'render_feedback': workflow.render_feedback,
    'review_material': workflow.review_material, 'matcher': matcher,
    'unified': lambda a, b, x, y: ''.join(difflib.unified_diff(a, b, x, y)), 'repr': repr,
    'positive': workflow.positive, 'role_backend': lambda name, opts: workflow.role_backend(harness.backend(name), opts),
    'load_spec': workflow.load_spec, 'digest': workflow.digest, 'write_json': write_json,
    'write_files': write_files, 'listing': listing,
    'spec_test_file': lambda t: list(workflow.spec_test_file(t)),
    'test_sources': lambda s: list(workflow.test_sources(s).items()),
}
req = json.loads(sys.stdin.buffer.read().decode('utf-8'))
fn = H[req['fn']]
out = []
for args in req['cases']:
    try:
        out.append(json.dumps({'ok': fn(*args)}))
    except BaseException as e:
        out.append(json.dumps({'err': [type(e).__name__, str(e)]}))
sys.stdout.write(json.dumps(out))
"#;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// The Python results for a batch of argument lists, each as json.dumps({'ok': result} or {'err': [type, str]}).
fn python(function: &str, cases: &[Value]) -> Vec<String> {
    python_with(function, cases, &["-I", "-B"])
}

/// python() with these interpreter flags (main's colour checks need the environment, which -I ignores).
fn python_with(function: &str, cases: &[Value], flags: &[&str]) -> Vec<String> {
    let request = pyjson::dumps(&json!({ "fn": function, "cases": cases }));
    let mut child = Command::new(ringo_cli::python())
        .args(flags)
        .args(["-c", DRIVER])
        .arg(root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 runs");
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(request.as_bytes()).unwrap());
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(out.status.success(), "driver failed: {}", String::from_utf8_lossy(&out.stderr));
    let results: Vec<String> = serde_json::from_slice(&out.stdout).expect("driver output");
    assert_eq!(results.len(), cases.len());
    results
}

fn enc(result: Result<Value, Error>) -> String {
    match result {
        Ok(v) => pyjson::dumps(&json!({ "ok": v })),
        Err(e) => pyjson::dumps(&json!({ "err": [e.type_name(), e.msg] })),
    }
}

fn short(s: &str) -> String {
    let mut t: String = s.chars().take(600).collect();
    if t.len() < s.len() {
        t.push_str("...");
    }
    t
}

/// Runs the batch through Python and Rust; panics with the first mismatches. Returns the case count.
fn gate(function: &str, cases: &[Value], rust: impl Fn(&Value) -> String) -> usize {
    let want = python(function, cases);
    let mut bad = Vec::new();
    for (case, want) in cases.iter().zip(&want) {
        let got = rust(case);
        if &got != want {
            let input = pyjson::dumps(case);
            bad.push(format!("input ({} bytes): {}\n  python: {}\n  rust:   {}", input.len(), short(&input), short(want), short(&got)));
        }
    }
    assert!(bad.is_empty(), "{function}: {} of {} differ:\n{}", bad.len(), cases.len(), bad[..bad.len().min(6)].join("\n"));
    let ok = want.iter().filter(|w| w.starts_with("{\"ok\"")).count();
    eprintln!("{function}: {} cases identical ({ok} results, {} errors)", cases.len(), cases.len() - ok);
    cases.len()
}

fn files(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
    fn text(&mut self, alphabet: &[&str], max: usize) -> String {
        (0..self.below(max + 1)).map(|_| *self.pick(alphabet)).collect()
    }
}

// ------------------------------------------------------------------------------------------------------------
// strict_json

const JSON_FIXTURES: &[&str] = &[
    "", " ", "{}", "[]", "{\"a\":1}", "[1,]", "{\"a\":1,}", "{", "[", "\"abc", "\"\\x\"", "\"\\u12\"", "\"\\u12x4\"",
    "\"a\u{1}\"", "nul", "tru", "fals", "null ", " null", "nulll", "-", "-x", "1.", "1e", "1e+", "1E+5", "1.5e-3",
    "-0", "-0.0", "0.1", "00", "01", "[1 2]", "{\"a\" 1}", "{\"a\":1 \"b\":2}", "{1:2}", "\u{feff}{}", " \u{feff}{}",
    "{\"a\":[1,}", "[,]", "{,}", "{\"a\":}", "\"\\ud83d\\ude00\"", "\"\\ud83d\"", "\"\\ude00\"", "\"\\ud83d\\u0041\"",
    "\"\\ud83dx\"", "\"\\ud83d\\uzzzz\"", "\"\\ud83d\\u12\"", "\"\\ud83d\\ud83d\\ude00\"", "\"a\\udc00\\udc00b\"",
    "{\"é\": \"😀\"}", "\"\t\"", "\"\\/\"", "\"\\b\\f\\n\\r\\t\"", "{\r\n\"a\"\r\n:\r\n1\r\n}", "{\"a\":1}\n\n x",
    "[1]\n\n\n]", "éééé x", "[\"é\", 1 2]", "{\"files\": {}, \"files\": {}}", "{\"x\": NaN}", "{\"x\": 1e400}",
    "-Infinity", "Infinity", "NaN", "[-Infinity]", "[-Inf]", "{\"a\":{\"b\":1,\"b\":[NaN]}}", "{\"a\":[1e999], \"a\":2}",
    "\"\\u00e9\"", "\"\\uD83D\\uDE00\"", "\"\\u", "\"\\u0041", "\"\\", "\"abc\\", "1.5E", "-01", "2.", ".5", "+1",
    "[1, 2, 3]   ", "\n\n\"a\nb\"", "{\"a\":1}{}", "[\"a\" : 1]", "{\"a\",1}", "[true false]", "{\"\":\"\"}",
    "{\"a\": 1, \"b\": {\"a\": 2}, \"c\": [{\"a\": 3}, {\"a\": 4}]}", "123456789012345678901234567890",
    "1.7976931348623157e308", "1.7976931348623159e308", "4.9e-324", "1e-400", "-1e-400", "0e0", "0E+0",
];

fn json_scalar(r: &mut Rng) -> String {
    let nums = ["0", "-0", "1", "-12", "3.25", "1e5", "1E-3", "-0.0", "12345678901234567890", "1.5e300", "2.5E+2", "7"];
    let strs = ["\"\"", "\"a\"", "\"é\"", "\"😀\"", "\"\\n\"", "\"\\u00e9\"", "\"\\ud83d\\ude00\"", "\"\\\"\"", "\"a b\"", "\"\\/\""];
    match r.below(5) {
        0 => r.pick(&["null", "true", "false"]).to_string(),
        1 | 2 => r.pick(&nums).to_string(),
        _ => r.pick(&strs).to_string(),
    }
}

fn json_value(r: &mut Rng, depth: usize) -> String {
    let ws = ["", "", " ", "\n", "\t", "\r\n", "  "];
    let w = |r: &mut Rng| r.pick(&ws).to_string();
    if depth == 0 || r.chance(40) {
        return json_scalar(r);
    }
    let n = r.below(4);
    if r.chance(50) {
        let items: Vec<String> = (0..n).map(|_| format!("{}{}{}", w(r), json_value(r, depth - 1), w(r))).collect();
        format!("[{}{}]", items.join(","), w(r))
    } else {
        let keys = ["\"a\"", "\"b\"", "\"files\"", "\"é\"", "\"\"", "\"rationale\""];
        let items: Vec<String> = (0..n)
            .map(|_| format!("{}{}{}:{}{}", w(r), r.pick(&keys), w(r), w(r), json_value(r, depth - 1)))
            .collect();
        format!("{{{}{}}}", items.join(","), w(r))
    }
}

fn mutate(r: &mut Rng, text: &str) -> String {
    let set = ["{", "}", "[", "]", ",", ":", "\"", "\\", "u", "0", "1", "e", "-", ".", "n", "N", "I", " ", "\n", "\u{1}", "é", "😀", "x"];
    let mut c: Vec<String> = text.chars().map(String::from).collect();
    for _ in 0..r.below(3) + 1 {
        let at = r.below(c.len() + 1);
        match r.below(3) {
            0 if at < c.len() => {
                c.remove(at);
            }
            1 if at < c.len() => c[at] = r.pick(&set).to_string(),
            _ => c.insert(at, r.pick(&set).to_string()),
        }
    }
    c.concat()
}

fn strict_rust(case: &Value) -> String {
    match wf::strict_json(s(&case[0])) {
        Err(e) if e.type_name() == "UnicodeEncodeError" => enc(Ok(json!({ "__surrogate__": true }))),
        other => enc(other),
    }
}

#[test]
fn strict_json_matches_python() {
    let mut r = Rng(0x5eed_0001);
    let mut cases: Vec<Value> = JSON_FIXTURES.iter().map(|t| json!([t])).collect();
    for _ in 0..4000 {
        let text = json_value(&mut r, 4);
        let text = if r.chance(70) { mutate(&mut r, &text) } else { text };
        cases.push(json!([text]));
    }
    cases.push(json!(["1".repeat(4300)]));
    cases.push(json!(["1".repeat(4301)]));
    cases.push(json!([format!("-{}", "1".repeat(4301))]));
    cases.push(json!([format!("[{}.5]", "1".repeat(5000))]));
    gate("strict_json", &cases, strict_rust);
}

fn nest(open: &str, inner: &str, close: &str, n: usize) -> String {
    format!("{}{inner}{}", open.repeat(n), close.repeat(n))
}

const NESTS: [(&str, &str, &str); 7] = [
    ("[", "", "]"),
    ("{\"a\":", "0", "}"),
    ("[", "NaN", "]"),
    ("[", "{}", "]"),
    ("[", "1.5", "]"),
    ("[", "{\"a\":1,\"a\":2}", "]"),
    ("[", "{\"a\":{\"b\":1}}", "]"),
];

/// Python's nesting limit is a C-stack check, so it moves by a few levels with the stack the caller already uses;
/// Rust's is fixed. Away from the limit both agree exactly; at it they agree within 64 levels.
#[test]
fn strict_json_nesting_limits_match_python() {
    // CPython's limit is its C stack, so it differs by machine (116,128 on CI's macOS, 116,223 here, lower on Linux);
    // the port uses a fixed one. Depths far from every limit are compared here; the boundary is measured by
    // `strict_json_nesting_boundary_is_near_python`, run by hand with --ignored.
    let mut cases = Vec::new();
    for (open, inner, close) in NESTS {
        for n in [1000, 200_000] {
            cases.push(json!([nest(open, inner, close, n)]));
        }
    }
    cases.push(json!([format!("{}x", "[".repeat(200_000))]));
    cases.push(json!([format!("[{}]", "1,".repeat(1000) + "1")]));
    gate("strict_kind", &cases, |case| {
        enc(wf::strict_json(s(&case[0])).map(|v| {
            pyjson::dispose(v);
            json!("ok")
        }))
    });
}

#[test]
#[ignore = "measures this machine's CPython stack limit; run by hand"]
fn strict_json_nesting_boundary_is_near_python() {
    let deep = |text: &str| match wf::strict_json(text) {
        Ok(v) => {
            pyjson::dispose(v); // a recursive drop of a value this deep would overflow the stack
            false
        }
        Err(e) => e.msg == "JSON nesting exceeds the parser limit",
    };
    let cases: Vec<Value> = NESTS.iter().map(|(o, i, c)| json!([o, i, c])).collect();
    let want = python("json_boundary", &cases);
    for ((open, inner, close), want) in NESTS.iter().zip(want) {
        let (mut lo, mut hi) = (1usize, 400_000usize);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if deep(&nest(open, inner, close, mid)) { hi = mid - 1 } else { lo = mid }
        }
        let py: usize = serde_json::from_str::<Value>(&want).unwrap()["ok"].as_u64().unwrap() as usize;
        eprintln!("nesting limit for {open}{inner}{close}: python {py}, rust {lo}");
        assert!(py.abs_diff(lo) <= 64, "{open}{inner}{close}: python {py}, rust {lo}");
    }
}

// ------------------------------------------------------------------------------------------------------------
// file_map, near_miss, materialize

#[test]
fn file_map_matches_python() {
    let names = [
        "a", ".", "..", "a//b", "./a", "a/", "/a", "\\a", "a\\b", ".git/x", "a/.b", "", "é/ü.py", "//a", "a/./b",
        "a/../b", "...", "a.", "x/y/z.md", " ", "a b", "a\nb", "😀",
    ];
    let mut cases: Vec<Value> = names.iter().map(|n| json!([{ *n: "x" }, "files"])).collect();
    for bad in [json!(null), json!([]), json!("a"), json!({"a": 1}), json!({"a": null}), json!({"a": "x", "b": ["y"]})] {
        cases.push(json!([bad, "candidate files"]));
    }
    cases.push(json!([{ "a": "x".repeat(2_000_000) }, "files"]));
    cases.push(json!([{ "a": "x".repeat(2_000_001) }, "files"]));
    cases.push(json!([{ "a": "é".repeat(1_000_001) }, "files"]));
    cases.push(json!([{ "a": "x".repeat(1_000_000), "b": "y".repeat(1_000_001) }, "files"]));
    cases.push(json!([{ "a": "x".repeat(1_000_000), "b": "y".repeat(1_000_000) }, "files"]));
    gate("file_map", &cases, |c| enc(wf::file_map(&c[0], s(&c[1])).map(|m| Value::Object(m.clone()))));
}

#[test]
fn near_miss_matches_python() {
    let mut r = Rng(0x5eed_0002);
    let alphabet = ["a", "b", "\n", "c", "é", "😀", " ", "'", "\"", "\\", "\u{200b}", "\u{feff}", "\u{3000}", "\t"];
    let mut cases = vec![json!(["", "x"]), json!(["abc", "x"]), json!(["abc\ndef", "dex"]), json!(["a".repeat(500), "a".repeat(501)])];
    cases.push(json!([format!("x\n{}y", "é".repeat(300)), format!("{}z", "é".repeat(250))]));
    for _ in 0..3000 {
        let text = r.text(&alphabet, 40);
        let old = if r.chance(60) && !text.is_empty() {
            let c: Vec<char> = text.chars().collect();
            let a = r.below(c.len());
            let b = a + r.below(c.len() - a) + 1;
            let mut old: String = c[a..b].iter().collect();
            old.push_str(&r.text(&alphabet, 3));
            old
        } else {
            r.text(&alphabet, 8)
        };
        if !old.is_empty() {
            cases.push(json!([text, old]));
        }
    }
    gate("near_miss", &cases, |c| enc(Ok(Value::String(wf::near_miss(s(&c[0]), s(&c[1]))))));
}

fn edit(path: &str, old: &str, new: &str) -> Value {
    json!({ "path": path, "old": old, "new": new })
}

#[test]
fn materialize_matches_python() {
    let mut r = Rng(0x5eed_0003);
    let current = json!({"a.txt": "aaa\nxyz\n", "b.txt": "one two\n"});
    let fixed = [
        json!({"edits": [edit("a.txt", "aa", "b")], "rationale": "r"}),
        json!({"edits": [edit("a.txt", "aaa", "c"), edit("a.txt", "a\nx", "d")], "rationale": "r"}),
        json!({"edits": [edit("a.txt", "xyz", "NEW"), edit("a.txt", "NEW", "z")], "rationale": "r"}),
        json!({"edits": [edit("b.txt", "zz", "")], "rationale": "r"}),
        json!({"edits": [edit("../a.txt", "aaa", "b")], "rationale": "r"}),
        json!({"edits": [edit("b.txt", "", "x")], "rationale": "r"}),
        json!({"edits": [{"path": "a.txt", "old": "x"}], "rationale": "x"}),
        json!({"edits": [edit("a.txt", "xyz", "q")], "files": {}, "rationale": "x"}),
        json!({"files": {"a.txt": "x"}, "rationale": "x"}),
        json!({"edits": [edit("a.txt", "xyz", &"x".repeat(2_000_001))], "rationale": "r"}),
        json!({"edits": {}, "rationale": "x"}),
        json!({"edits": [edit("a.txt", "xyz", "XYZ"), edit("a.txt", "aaa", "A")], "rationale": "r"}),
        json!({"edits": [edit("a.txt", "xyz", "1"), edit("a.txt", "xyz", "0")], "rationale": "r"}),
        json!({"edits": [edit("a.txt", "xyz", "1"), edit("a.txt", "xyz", "1")], "rationale": "r"}),
        json!({"edits": [], "rationale": "r"}),
        json!({"files": {"a.txt": "1", "b.txt": "2"}, "rationale": "r"}),
        json!({"files": {"b.txt": "2", "a.txt": "1"}, "rationale": "r"}),
        json!({"files": {"a.txt": 1, "b.txt": "2"}, "rationale": "r"}),
        json!({"files": [], "rationale": "r"}),
        json!({"files": {"a.txt": "1", "b.txt": "2"}, "rationale": 5}),
        json!({"files": {"a.txt": "1", "b.txt": "2"}}),
        json!({"edits": [edit("a.txt", "xyz", "1")], "rationale": "r", "x": 1}),
        json!([]),
        json!("text"),
        json!(null),
        json!({"edits": [[]], "rationale": "r"}),
        json!({"edits": [{"path": "a.txt", "old": "x", "new": 5}], "rationale": "r"}),
        json!({"edits": [{"path": "a.txt", "old": "x", "new": "y", "z": "w"}], "rationale": "r"}),
    ];
    let mut cases: Vec<Value> = fixed.iter().map(|reply| json!([current, current, reply])).collect();
    let paths = ["a.txt", "b.txt", "c.txt", "../a.txt"];
    let alphabet = ["a", "b", "\n", "é", "😀"];
    for _ in 0..3000 {
        let cur = json!({"a.txt": r.text(&alphabet, 14), "b.txt": r.text(&alphabet, 10)});
        let reply = if r.chance(15) {
            let mut f = Map::new();
            for p in paths {
                if r.chance(60) {
                    f.insert(p.into(), Value::String(r.text(&alphabet, 6)));
                }
            }
            json!({"files": f, "rationale": "r"})
        } else {
            let edits: Vec<Value> = (0..r.below(4))
                .map(|_| {
                    let path = *r.pick(&paths);
                    let text: Vec<char> = cur.get(path).and_then(Value::as_str).unwrap_or("").chars().collect();
                    let old: String = if r.chance(75) && !text.is_empty() {
                        let a = r.below(text.len());
                        text[a..a + 1 + r.below((text.len() - a).min(5))].iter().collect()
                    } else {
                        r.text(&alphabet, 3)
                    };
                    edit(path, &old, &r.text(&alphabet, 4))
                })
                .collect();
            json!({"edits": edits, "rationale": "r"})
        };
        cases.push(json!([cur, cur, reply]));
    }
    gate("materialize", &cases, |c| enc(wf::materialize(&files(&c[0]), &files(&c[1]), &c[2]).map(Value::Object)));
}

/// run()'s parse for the JSON protocol: strict_json, then materialize. A lone surrogate in a file's text fails with
/// the UnicodeEncodeError Python's file_map raises; elsewhere Python keeps it (see strict_json).
#[test]
fn json_protocol_parse_matches_python() {
    let current = json!({"a.py": "x = 1\n", "b.py": ""});
    let mut cases = Vec::new();
    for text in [
        r#"{"files": {"a.py": "x\ud800y", "b.py": ""}, "rationale": "r"}"#,
        r#"{"files": {"a.py": "\udc80\udc81z", "b.py": ""}, "rationale": "r"}"#,
        r#"{"files": {"a.py": "ok", "b.py": "\ud83d\ude00\udbff"}, "rationale": "r"}"#,
        r#"{"edits": [{"path": "a.py", "old": "x = 1", "new": "y"}], "rationale": "r"}"#,
        r#"{"edits": [{"path": "a.py", "old": "x = 2", "new": "y"}], "rationale": "r"}"#,
        r#"{"files": {"a.py": "x", "b.py": ""}, "rationale": "r", "files": {}}"#,
        "not json", "[1]", "{\"rationale\": \"r\"}",
    ] {
        cases.push(json!([current, current, text]));
    }
    gate("json_protocol", &cases, |c| {
        enc(wf::strict_json(s(&c[2])).and_then(|candidate| {
            wf::materialize(&files(&c[0]), &files(&c[1]), &candidate).map(|f| json!([candidate, f]))
        }))
    });
    // The known deviation: Python keeps a lone surrogate outside file text (here the rationale); Rust refuses it.
    let e = wf::strict_json(r#"{"files": {"a.py": "x", "b.py": ""}, "rationale": "\ud800"}"#).unwrap_err();
    let want = "'utf-8' codec can't encode character '\\ud800' in position 0: surrogates not allowed";
    assert_eq!((e.type_name(), e.msg.as_str()), ("UnicodeEncodeError", want));
}

// ------------------------------------------------------------------------------------------------------------
// parse_text_reply and materialize_text

const REPLY_LINES: &[&str] = &[
    "@@@ FILE a.py", "@@@ FILE b.py", "@@@ FILE c.py", "@@@ FILE", "@@@ REPLACE a.py", "@@@ REPLACE b.py",
    "@@@ REPLACE", "@@@ WITH", "@@@ WITH x", "@@@ END", "@@@ END junk", "@@@", "@@@ RATIONALE", "@@@ rationale",
    "@@@ SCRIPT", "@@@ DISPUTE T.test_x", "@@@ DISPUTE", "@@@ SHELL rm", "@@@ ", "@@@  FILE  a.py  ", "@@@ ﬁle a.py",
    "@@@x", "```", "```python", "x = 1", "y = 2", "x = 2", "", "   ", "é😀 = 3", "def f(x):", "    return 2*x",
    "@@@ file b.py", "\t@@@ FILE a.py", "@@@ FILE a.py b.py", "@@@ FILE ../a.py", "@@@ dispute T.b reason",
    "@@@ ǆ", "@@@ ß", "x = 1  ", "@@@\u{2028}", "@@@ FILE\u{a0}a.py",
];

fn reply_text(r: &mut Rng) -> String {
    let seps = ["\n", "\n", "\n", "\n", "\r\n", "\r", "\u{2028}", "\u{c}", "\u{85}"];
    let n = r.below(9);
    let sep = *r.pick(&seps);
    let lines: Vec<&str> = (0..n).map(|_| *r.pick(REPLY_LINES)).collect();
    let mut text = lines.join(if r.chance(85) { sep } else { "\n" });
    if r.chance(30) {
        text = format!("{}{text}", r.pick(&["Sure.\n", "  \n", "\n\n", " "]));
    }
    if r.chance(40) {
        text.push_str(r.pick(&["\n", "\n\n", "  ", "\r\n"]));
    }
    text
}

fn json_reply(r: &mut Rng) -> String {
    let field = |r: &mut Rng| -> Value {
        r.pick(&[
            json!({"a.py": "x\n"}), json!({"c.py": "x"}), json!({"a.py": 5}), json!(["a.py"]), json!(["ab"]),
            json!([["a.py", "z"]]), json!([["a.py", 1]]), json!([[1, "z"]]), json!([[[1], "z"]]), json!([5]),
            json!([{"a.py": 1, "b.py": 2}]), json!("ab"), json!(""), json!(5), json!(null), json!(true), json!({}),
            json!([]), json!([[]]), json!(["a.py", "b.py"]), json!({"b.py": "y"}), json!(1.5),
        ])
        .clone()
    };
    let edits = |r: &mut Rng| -> Value {
        r.pick(&[
            json!([]), json!([{"path": "a.py", "old": "x = 1", "new": "x = 3"}]), json!([{"path": "b.py", "old": "q", "new": ""}]),
            json!([{"old": "x"}]), json!(["a.py"]), json!("a.py"), json!({"path": "a.py"}), json!([5]), json!([null]),
            json!([[1]]), json!(5), json!(null), json!([{"path": ["a.py"], "old": "x", "new": "y"}]),
            json!([{"path": 5, "old": "x", "new": "y"}]),
        ])
        .clone()
    };
    let mut reply = Map::new();
    for key in ["rationale", "files", "edits", "disputes", "other"] {
        if r.chance(60) {
            let v = match key {
                "files" => field(r),
                "edits" => edits(r),
                "rationale" => r.pick(&[json!("why"), json!(5), json!(null), json!(["x"])]).clone(),
                _ => json!({"T.t": "x"}),
            };
            reply.insert(key.into(), v);
        }
    }
    let text = pyjson::dumps(&Value::Object(reply));
    if r.chance(10) { mutate(r, &text) } else { text }
}

#[test]
fn parse_text_reply_and_materialize_text_match_python() {
    let mut r = Rng(0x5eed_0004);
    let fixtures = [
        "Sure.\n@@@ RATIONALE\nDouble it.\n@@@ FILE a.py\ndef f(x):\n    return 2*x\n\n@@@ REPLACE b.py\nx = 1\n\n@@@ WITH\nx = 2\n@@@ END\n@@@ DISPUTE Brief.test_odd\n\"Return twice x.\"\n",
        "@@@ FILE a.py\nx = 1\n@@@ END\n@@@ FILE b.py\ny = 2\n@@@ END\n",
        "```\n@@@ FILE a.py\nx\n```",
        "{\"files\": {\"a.py\": \"y\\n\"}, \"rationale\": \"r\"}",
        "@@@ REPLACE a.py\nx = 1\n@@@\nx = 2\n@@@ END\n@@@ FILE b.py\ny\n@@@\n",
        "no blocks at all", "@@@ REPLACE a.py\nx\n@@@ END\n", "@@@ SHELL rm -rf /\n", "@@@ FILE other.py\nx\n",
        "@@@ FILE a.py\nx\n@@@ REPLACE a.py\nx\n@@@ WITH\ny\n@@@ END\n", "@@@ SCRIPT\nprint(1)\n@@@ SCRIPT\nprint(2)\n",
        "@@@ SCRIPT\n\n\nprint(1)\n\n", "```", "```\n```", "", "   ", "{", "{\"a\": 1} x", "{\"rationale\": \"\\ud800\"}",
        "@@@ REPLACE a.py\r\nx = 1\r\n@@@ WITH\r\nx = 9\r\n@@@ END\r\n", "@@@ REPLACE a.py\nx = 1\n@@@ WITH\nx = 9\n@@@ END\n",
        "@@@ REPLACE a.py\nnot there\n@@@ WITH\nx\n@@@ END\n", "@@@ REPLACE a.py\nx = 1\n@@@ WITH\n@@@ END\n",
        "@@@ REPLACE a.py\nx = 1\n@@@ WITH\nz\n@@@ END\nstray\n",
    ];
    let mut cases = Vec::new();
    let currents = [
        json!({"a.py": "x = 1\ny = 2\n", "b.py": "x = 1\ny = 0\n", "REPORT.md": ""}),
        json!({"a.py": "x = 1\nx = 1\n", "b.py": "", "REPORT.md": "total: 6\n"}),
        json!({"a.py": "", "b.py": "é😀 = 3\nx = 1\n", "REPORT.md": "x = 1"}),
    ];
    for f in fixtures {
        for c in &currents {
            cases.push(json!([c, c, f]));
        }
    }
    for _ in 0..6000 {
        let text = if r.chance(15) { json_reply(&mut r) } else { reply_text(&mut r) };
        let c = r.pick(&currents).clone();
        cases.push(json!([c, c, text]));
    }
    gate("text_pipeline", &cases, |c| {
        let (current, declared) = (files(&c[0]), files(&c[1]));
        enc(match wf::parse_text_reply(s(&c[2])) {
            Err(e) if e.type_name() == "UnicodeEncodeError" => Ok(json!({ "__surrogate__": true })),
            Err(e) => Err(e),
            Ok(reply) => Ok(match wf::materialize_text(&current, &declared, &reply) {
                Ok(f) => json!({ "reply": reply, "files": f }),
                Err(e) => json!({ "reply": reply, "error": [e.type_name(), e.msg] }),
            }),
        })
    });
}

#[test]
fn block_kinds_upper_case_as_python_does() {
    // '@@@ ' + a run of non-space code points: the error names the kind as str.upper() wrote it.
    let mut cases = Vec::new();
    let mut chunk = String::new();
    for cp in 0x21u32..0x110000 {
        let Some(c) = char::from_u32(cp) else { continue };
        if c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c) || matches!(c, '\u{85}' | '\u{2028}' | '\u{2029}') {
            continue;
        }
        chunk.push(c);
        if chunk.chars().count() == 512 {
            cases.push(json!([format!("@@@ {chunk}\n")]));
            chunk.clear();
        }
    }
    cases.push(json!([format!("@@@ {chunk}\n")]));
    gate("parse_text_reply", &cases, |c| enc(wf::parse_text_reply(s(&c[0]))));
}

// ------------------------------------------------------------------------------------------------------------
// numbers, repr, artifact_kind, positive, text_of

/// str(x) and float(x), the float as its bits (pyjson::float rounds a shortest-repr tie up where repr rounds it to
/// even, e.g. 1566004145404016.25, so a printed float would compare the formatter, not the conversion).
fn num_row(x: &wf::Num) -> Value {
    json!([x.to_string(), x.to_f64().to_bits()])
}

#[test]
fn constants_match_python() {
    let want = python("constants", &[json!([])]);
    let got = enc(Ok(json!([wf::LIMIT, wf::MARK, wf::REPAIR, wf::ROLES, wf::EFFORTS, wf::DOC_SUFFIXES, wf::NUMBER])));
    assert_eq!(got, want[0]);
}

#[test]
fn numbers_match_python() {
    let mut r = Rng(0x5eed_0005);
    let alphabet = ["-", "\u{2212}", "1", "0", "9", "5", ",", ".", "a", "Z", "_", " ", "é", "٣", "\u{301}", "²", "x", "\n", "S", "١", "𝟗"];
    let mut cases: Vec<Value> = [
        "2.00% of 1,628.62 at 09:05 [S4]", "profit: 6", "profit: -6", "loss −6.22", "2031-09-16 and 10-20",
        "2031 09 16 10 20", "", "-0 and 0", "0 and -0", "0.000 -0.0", "100 1e5 1.5.5", "a-5 .-5 _-5 é-5 x\u{301}-5 ²-5",
        "1234567890123456789012345678", "12345678901234567890123456789", "12345678901234567890123456785",
        "12345678901234567890123456775", "99999999999999999999999999995", "99999999999999999999999999999",
        "0.0000001 0.000001 123.4500 1,000,000 1,, 1,.5 -.5 5.", "٣٤.٥ ١٢٣ 𝟗𝟗",
    ]
    .iter()
    .map(|t| json!([t]))
    .collect();
    cases.push(json!([format!("0.{}1", "0".repeat(1_000_030))]));
    cases.push(json!([format!("0.{}1", "0".repeat(1_000_020))]));
    cases.push(json!([format!("0.{}15", "0".repeat(1_000_024))]));
    cases.push(json!([format!("0.{}25", "0".repeat(1_000_024))]));
    cases.push(json!(["9".repeat(1_000_000)]));
    cases.push(json!(["1".repeat(999_999)]));
    cases.push(json!(["1".repeat(1_000_001)]));
    cases.push(json!([format!("1{}", "0".repeat(999_999))]));
    cases.push(json!([format!("1{}", "0".repeat(1_000_000))]));
    cases.push(json!([format!("{}.5 1{}", "9".repeat(28), "0".repeat(330))]));
    cases.push(json!(["0.1000000000000000000000000001 0.1 2.5000000000000000000000000005 1e5"]));
    for _ in 0..4000 {
        cases.push(json!([r.text(&alphabet, 30)]));
    }
    for _ in 0..1000 {
        let digits = ["0", "1", "4", "5", "6", "9"];
        cases.push(json!([format!("{} {}.{}", r.text(&digits, 34), r.text(&digits, 20), r.text(&digits, 20))]));
    }
    gate("numbers", &cases, |c| {
        enc(wf::numbers(s(&c[0])).map(|n| Value::Array(n.iter().map(num_row).collect())))
    });
}

#[test]
fn repr_matches_python_for_every_code_point() {
    let mut cases = Vec::new();
    let mut chunk = String::new();
    for cp in 0u32..0x110000 {
        if let Some(c) = char::from_u32(cp) {
            chunk.push(c);
        }
        if cp % 0x800 == 0x7ff {
            cases.push(json!([chunk]));
            chunk.clear();
        }
    }
    for t in ["", "'", "\"", "'\"", "it's", "a\\b", "\u{0}\u{7f}\u{80}\u{a0}\u{ad}", "😀\u{e000}\u{10ffff}"] {
        cases.push(json!([t]));
    }
    gate("repr", &cases, |c| enc(Ok(Value::String(ringo_cli::rt::py::repr(s(&c[0]))))));
}

#[test]
fn artifact_kind_positive_and_text_of_match_python() {
    let names = ["a.md", "A.MD", "a.txt", "notes.rst", "x.adoc", "a.py", ".md", "..md", "a.", "a.md.", "a.b.md", "d/x.md", "x", "a.Md", "a.ＭＤ", "a.\u{130}d"];
    let mut cases = Vec::new();
    for a in names {
        cases.push(json!([{"files": {a: ""}}]));
        for b in names {
            cases.push(json!([{"files": {a: "", b: ""}}]));
        }
    }
    cases.push(json!([{"files": {}}]));
    cases.push(json!([{}]));
    cases.push(json!([{"files": [1]}]));
    gate("artifact_kind", &cases, |c| enc(wf::artifact_kind(&c[0]).map(|k| json!(k))));

    let mut cases = Vec::new();
    for v in [json!(1), json!(0), json!(-1), json!(1.5), json!(0.0), json!(-0.0), json!(true), json!(false), json!(null), json!("3"), json!([1]),
              json!(1e-300), json!(100000000000000000000000000000_u128), json!(2.0)] {
        for integer in [false, true] {
            cases.push(json!([v, "max_rounds", integer]));
        }
    }
    gate("positive", &cases, |c| enc(wf::positive(&c[0], s(&c[1]), c[2].as_bool().unwrap()).cloned()));

    let cases = vec![json!([{}]), json!([{"a": "x"}]), json!([{"a": "x", "b": "", "c": "é\n"}])];
    gate("text_of", &cases, |c| enc(Ok(Value::String(wf::text_of(&files(&c[0]))))));
}

// ------------------------------------------------------------------------------------------------------------
// text_message and render_feedback

fn log(r: &mut Rng) -> String {
    match r.below(4) {
        0 => String::new(),
        1 => "ok\n".into(),
        2 => format!("{}é😀\n", "line\n".repeat(r.below(1500))),
        _ => "x".repeat(5990 + r.below(20)),
    }
}

fn check(r: &mut Rng) -> Value {
    let status = *r.pick(&["pass", "fail", "timeout"]);
    json!({"name": r.pick(&["reference", "it's", "lint \"x\"", "é"]), "status": status, "returncode": 1,
           "seconds": 0.5, "stdout": log(r), "stderr": log(r)})
}

fn test_row(r: &mut Rng) -> Value {
    json!({"id": r.pick(&["Brief.test_a", "T.test_é"]), "error": log(r), "source": "def test_a(self):\n    pass"})
}

fn feedback(r: &mut Rng) -> Value {
    let mut f = Map::new();
    if r.chance(30) {
        f.insert("protocol_error".into(), json!(r.pick(&["edit 1 (a.py): old matches 0 places", ""])));
    }
    if r.chance(60) {
        f.insert("checks".into(), Value::Array((0..r.below(3)).map(|_| check(r)).collect()));
    }
    if r.chance(30) {
        let failing: Vec<Value> = (0..r.below(3)).map(|_| test_row(r)).collect();
        f.insert("spec_tests".into(), json!({"status": "fail", "passed": r.below(5), "total": 5, "failing": failing, "failing_ids": []}));
    }
    if r.chance(25) {
        let checks: Vec<Value> = (0..r.below(3)).map(|_| check(r)).collect();
        let tests: Vec<Value> = (0..r.below(2)).map(|_| test_row(r)).collect();
        let unbacked = r.pick(&[json!([]), json!(["9"]), json!(["9", "1E+2", "-0.5"])]).clone();
        f.insert("regression".into(), json!({"checks": checks, "tests": tests, "unbacked": unbacked}));
    }
    if r.chance(25) {
        f.insert("script".into(), json!({"status": r.pick(&["pass", "fail"]), "stdout": log(r), "stderr": log(r), "source": "print(1)\n"}));
    }
    if r.chance(30) {
        f.insert("review".into(), json!(r.pick(&["### 1. Name\n- Problem: x.", ""])));
    }
    if r.chance(20) {
        f.insert("previous_review".into(), json!("Earlier: no defects found."));
    }
    if r.chance(10) {
        f.insert("previous_council".into(), json!({"decision": {"action": "escalate"}}));
    }
    Value::Object(f)
}

#[test]
fn text_message_and_render_feedback_match_python() {
    let mut r = Rng(0x5eed_0006);
    let mut cases = Vec::new();
    for _ in 0..1500 {
        let mut spec = json!({"brief": r.pick(&["Implement f(x)=2*x.\n\nHard constraints:\n- Return twice x.\n", "b", "é😀"]),
                              "fixtures": r.pick(&[json!({}), json!({"check.py": "assert f(5) == 10\n", "data.csv": "a,b\n1,2\n"})])});
        if r.chance(50) {
            spec["material"] = json!(r.pick(&["", "Source S1: 1,628.62", "m"]));
        }
        let files = r.pick(&[json!({"value.py": ""}), json!({"a.py": "x = 1\n", "REPORT.md": "total: 6\n"})]).clone();
        let fb = if r.chance(15) { json!({}) } else { feedback(&mut r) };
        cases.push(json!([spec, files, fb]));
    }
    // The spec-test writer's message and malformed structures.
    cases.push(json!([{"brief": "b", "fixtures": {}}, {"value.py": ""}, {}]));
    cases.push(json!([{"brief": "b"}, {"value.py": ""}, {}]));
    cases.push(json!([{"brief": "b", "fixtures": []}, {}, {}]));
    gate("text_message", &cases, |c| enc(wf::text_message(&c[0], &files(&c[1]), &c[2]).map(Value::String)));

    let mut cases: Vec<Value> = (0..1500).map(|_| json!([feedback(&mut r)])).collect();
    for bad in [
        json!({"checks": [{"status": "fail", "name": "n", "stdout": "a"}]}),
        json!({"checks": [{"status": "fail", "name": "n", "stdout": "a", "stderr": 5}]}),
        json!({"checks": [{"status": "fail", "name": "n", "stdout": 5, "stderr": "a"}]}),
        json!({"checks": [{"name": "n"}]}),
        json!({"checks": ["pass"]}),
        json!({"checks": [5]}),
        json!({"checks": 5}),
        json!({"checks": {"a": 1}}),
        json!({"checks": [{"status": "pass"}]}),
        json!({"checks": [{"status": "fail", "name": 5, "stdout": "", "stderr": ""}]}),
        json!({"spec_tests": {"failing": [{"id": 1}], "passed": 1, "total": 2}}),
        json!({"spec_tests": {"failing": []}}),
        json!({"spec_tests": {"failing": [5], "passed": 1.5, "total": true}}),
        json!({"spec_tests": "x"}),
        json!({"regression": {"checks": [], "tests": [], "unbacked": "95"}}),
        json!({"regression": {"checks": [], "tests": [], "unbacked": [9]}}),
        json!({"regression": {"checks": [], "tests": []}}),
        json!({"regression": {"tests": []}}),
        json!({"script": {"status": "pass", "stdout": "", "stderr": ""}}),
        json!({"script": {"status": null, "stdout": "", "stderr": ""}}),
        json!({"review": 5, "previous_review": ["x", {"y": 1.0}]}),
        json!({"protocol_error": [1, "é", null, true, 2.5]}),
        json!([]),
        json!({}),
    ] {
        cases.push(json!([bad]));
    }
    gate("render_feedback", &cases, |c| enc(wf::render_feedback(&c[0]).map(Value::String)));
}

// ------------------------------------------------------------------------------------------------------------
// difflib and review_material

fn lines(r: &mut Rng, alphabet: usize, max: usize) -> Vec<String> {
    let n = r.below(max + 1);
    let mut out: Vec<String> = (0..n)
        .map(|_| {
            let k = r.below(alphabet);
            match r.below(10) {
                0 => format!("line {k}\r\n"),
                1 => format!("é {k}\n"),
                _ => format!("line {k}\n"),
            }
        })
        .collect();
    if r.chance(30)
        && let Some(last) = out.last_mut()
    {
        *last = last.trim_end().to_string();
    }
    out
}

fn edited(r: &mut Rng, base: &[String], alphabet: usize) -> Vec<String> {
    let mut out = base.to_vec();
    for _ in 0..r.below(6) {
        let at = r.below(out.len() + 1);
        match r.below(3) {
            0 if at < out.len() => {
                out.remove(at);
            }
            1 if at < out.len() => out[at] = format!("new {}\n", r.below(alphabet)),
            _ => out.insert(at, format!("ins {}\n", r.below(alphabet))),
        }
    }
    out
}

fn strs(v: &Value) -> Vec<&str> {
    v.as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect()
}

fn matcher_rust(c: &Value) -> String {
    let (a, b) = (strs(&c[0]), strs(&c[1]));
    let m = difflib::SequenceMatcher::new(&a, &b, c[2].as_bool().unwrap());
    let blocks: Vec<Value> = m.get_matching_blocks().into_iter().map(|(i, j, k)| json!([i, j, k])).collect();
    let op = |o: difflib::Opcode| json!([o.0, o.1, o.2, o.3, o.4]);
    let ops: Vec<Value> = m.get_opcodes().into_iter().map(op).collect();
    let groups: Vec<Value> = m.get_grouped_opcodes(3).into_iter().map(|g| Value::Array(g.into_iter().map(op).collect())).collect();
    enc(Ok(json!([blocks, ops, groups])))
}

fn diff_cases(r: &mut Rng, count: usize) -> Vec<(Vec<String>, Vec<String>)> {
    (0..count)
        .map(|_| {
            let alphabet = *r.pick(&[2, 5, 30, 400]);
            let max = *r.pick(&[5, 40, 260, 500]);
            let a = lines(r, alphabet, max);
            let b = if r.chance(70) { edited(r, &a, alphabet) } else { lines(r, alphabet, max) };
            (a, b)
        })
        .collect()
}

#[test]
fn difflib_matches_python() {
    let mut r = Rng(0x5eed_0007);
    let pairs = diff_cases(&mut r, 1200);
    let mut cases = Vec::new();
    for (a, b) in &pairs {
        cases.push(json!([a, b, true]));
        cases.push(json!([a, b, false]));
    }
    cases.push(json!([[], [], true]));
    cases.push(json!([["x\n"], [], true]));
    cases.push(json!([[], ["x"], true]));
    let popular: Vec<String> = (0..300).map(|i| if i % 3 == 0 { format!("{i}\n") } else { "same\n".into() }).collect();
    let mut changed = popular.clone();
    changed[150] = "other\n".into();
    cases.push(json!([popular, changed, true]));
    cases.push(json!([popular, changed, false]));
    gate("matcher", &cases, matcher_rust);

    let mut cases: Vec<Value> = pairs.iter().map(|(a, b)| json!([a, b, "a/x", "b/x"])).collect();
    cases.push(json!([popular, changed, "a/é b", "b/é b"]));
    gate("unified", &cases, |c| enc(Ok(Value::String(difflib::unified_diff(&strs(&c[0]), &strs(&c[1]), s(&c[2]), s(&c[3]))))));
}

#[test]
fn review_material_matches_python() {
    let mut r = Rng(0x5eed_0008);
    let mut cases = vec![
        json!([{"brief": "b", "files": {"a.py": "x = 1\ny = 2\n", "new.py": ""}, "fixtures": {}},
               {"a.py": "x = 1\ny = 3\n", "new.py": "z = 0\n"}, "r", []]),
        json!([{"brief": "b", "files": {"a.py": "def f(x):\n    raise NotImplementedError\n"}, "fixtures": {}},
               {"a.py": format!("def f(x):\n{}    return y9\n", (0..10).map(|i| format!("    y{i} = x + {i}\n")).collect::<String>())}, "r", []]),
        json!([{"brief": "b", "files": {"a.py": "x"}, "fixtures": {}}, {"a.py": "x"}, 5, []]),
        json!([{"brief": "b", "files": {"a.py": "x"}, "fixtures": {}}, {"b.py": "x"}, "r", []]),
        json!([{"brief": "b", "files": {"a.py": "x"}, "fixtures": {}}, {"a.py": "x"}, ["r", {"k": null}], [{"name": "n", "status": "pass", "stdout": "o"}]]),
    ];
    for _ in 0..800 {
        let alphabet = *r.pick(&[3, 30, 400]);
        let max = *r.pick(&[0, 8, 80, 300]);
        let a = lines(&mut r, alphabet, max);
        let b = if r.chance(75) { edited(&mut r, &a, alphabet) } else { lines(&mut r, alphabet, 40) };
        let mut spec = json!({"brief": "Fix f.", "files": {"a.py": a.concat(), "REPORT.md": ""},
                              "fixtures": r.pick(&[json!({}), json!({"check.py": "assert True\n"})])});
        if r.chance(40) {
            spec["material"] = json!(r.pick(&["", "Source S1: 6"]));
        }
        let observed: Vec<Value> = (0..r.below(3)).map(|_| check(&mut r)).collect();
        let report = if r.chance(50) { "total: 6\n" } else { "" };
        cases.push(json!([spec, {"a.py": b.concat(), "REPORT.md": report}, r.pick(&["r", "Targeted change.", ""]), observed]));
    }
    gate("review_material", &cases, |c| {
        let observed: Vec<Value> = c[3].as_array().unwrap().clone();
        enc(wf::review_material(&c[0], &files(&c[1]), &c[2], &observed).map(Value::String))
    });
}

// ------------------------------------------------------------------------------------------------------------
// load_spec, role_backend

struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Temp {
        let dir = std::env::temp_dir().join(format!("ringo-gate-workflow-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Temp(dir)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        if std::env::var_os("GATE_KEEP").is_none() {
            // GATE_KEEP=1 keeps the folders for a look at what each side wrote
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn base_spec(council: bool) -> Value {
    let mut spec = json!({"version": 1, "brief": "Implement f(x)=2*x.\n", "files": {"value.py": ""},
        "fixtures": {"check.py": "assert True\n"}, "checks": [{"name": "reference", "argv": ["{python}", "-I", "-B", "check.py"]}],
        "writer": "fake", "judge": "fake", "allow_tool_backends": true, "allow_execution": true, "max_rounds": 3});
    if council {
        spec["council"] = json!(["python3", "bridge.py"]);
        spec["rubric"] = json!("rubric.json");
        spec["samples"] = json!(2);
    }
    spec
}

fn spec_mutations() -> Vec<(&'static str, Option<Value>)> {
    let mut m: Vec<(&'static str, Option<Value>)> = Vec::new();
    let mut add = |key: &'static str, values: Vec<Value>| {
        m.push((key, None));
        m.extend(values.into_iter().map(|v| (key, Some(v))));
    };
    add("version", vec![json!(1), json!(1.0), json!(true), json!(2), json!("1"), json!(null), json!(1.0000000000000002)]);
    add("brief", vec![json!(""), json!("  "), json!(5), json!("x")]);
    add("material", vec![json!(""), json!("m"), json!(5), json!(null)]);
    add("files", vec![json!({}), json!({"a": ""}), json!({"a": "", "a/b": ""}), json!({"./a": ""}), json!({".": ""}), json!({".": "", "b": ""}),
        json!({"a": 5}), json!([]), json!({"../a": ""}), json!({"a//b": ""}), json!({"é.md": "ü"}), json!({"check.py": ""}), json!({"x/check.py/y": ""}),
        json!({"check.py/z": ""})]);
    add("fixtures", vec![json!({}), json!([]), json!({"value.py/x": ""}), json!({"a/b": "", "a": ""}), json!(null)]);
    add("checks", vec![json!([]), json!([{}]), json!([{"name": "c"}]), json!([{"name": "", "argv": ["x"]}]), json!([{"name": "c", "argv": []}]),
        json!([{"name": "c", "argv": [""]}]), json!([{"name": "c", "argv": ["x"], "timeout_s": 0}]), json!([{"name": "c", "argv": ["x"], "timeout_s": -1}]),
        json!([{"name": "c", "argv": ["x"], "timeout_s": 1.5}]), json!([{"name": "c", "argv": ["x"], "timeout_s": true}]),
        json!([{"name": "c", "argv": ["x"], "timeout_s": "x"}]), json!([{"name": "c", "argv": ["x"], "extra": 1}]), json!("x"),
        json!([{"name": "c", "argv": ["{python}"]}, {"name": "d", "argv": ["a", "{python}", "{python} "]}]),
        json!([{"name": "c", "argv": ["x"]}, {"name": "d", "argv": ["y"], "timeout_s": 0}]), json!([{"name": "c", "argv": ["x"]}, 5])]);
    for key in ["writer", "judge"] {
        add(key, vec![json!("claude"), json!("codex"), json!("codex-chatgpt"), json!("ollama:"), json!("ollama:llama3"), json!("claude:"),
            json!("claude:foo --model x"), json!("codex:codex exec -"), json!("codex:codex exec --json -"), json!("codex:codex --search --json"),
            json!("codex:'unclosed"), json!("plain \\"), json!(""), json!("a 'b c' \"d\\\"e\" f\\ g 'h\\i' \"j\\k\""), json!(5), json!("  "), json!("x\"")]);
    }
    for key in ["allow_tool_backends", "allow_execution", "protocol_repair", "no_regression"] {
        add(key, vec![json!(true), json!(false), json!(1), json!("yes")]);
    }
    add("max_rounds", vec![json!(1), json!(0), json!(-1), json!(1.5), json!(true), json!("3"), json!(100000000000000000000000000000_u128)]);
    for key in ["max_seconds", "timeout_s"] {
        add(key, vec![json!(0.5), json!(0), json!(-1), json!(true), json!(1e-300), json!(null)]);
    }
    add("protocol", vec![json!("text"), json!("json"), json!("xml"), json!(5), json!(null)]);
    add("roles", vec![json!({}), json!([]), json!({"writer": {}}), json!({"writer": {"model": "m"}}), json!({"writer": {"effort": "low"}}),
        json!({"writer": {"effort": "huge"}}), json!({"writer": {"model": ""}}), json!({"writer": {"model": 5}}), json!({"bogus": {}}),
        json!({"review": {"effort": "high"}}), json!({"writer": "x"}), json!({"writer": {"model": "m", "x": 1}}), json!({"tests": {"model": "m", "effort": "max"}})]);
    add("spec_tests", vec![json!(null), json!({}), json!({"python": "{python}"}), json!({"python": ""}), json!({"python": "p", "timeout_s": 0}),
        json!({"python": "p", "extra": 1}), json!("x"), json!({"python": "p", "timeout_s": 2.5}), json!({"python": 5})]);
    add("scripts", vec![json!(null), json!({}), json!({"python": "{python}"}), json!({"python": "p", "timeout_s": -2}), json!({"python": "p"})]);
    add("candidates", vec![json!(1), json!(4), json!(5), json!(0), json!(2.0), json!(true), json!(2)]);
    add("max_cost", vec![json!(1), json!(null), json!("x")]);
    add("unpriced_call_usd", vec![json!(0.1), json!(null)]);
    add("max_calls", vec![json!(9)]);
    add("bogus", vec![json!(1)]);
    add("council", vec![json!([]), json!(["x"]), json!([""]), json!("x"), json!([5]), json!(null)]);
    add("rubric", vec![json!(""), json!("r.json"), json!("/abs/r.json"), json!("../r.json"), json!("sub/../r.json"), json!("link/r.json"), json!("  ")]);
    add("samples", vec![json!(1), json!(2), json!(25), json!(26), json!(6), json!(true), json!(2.0), json!(7)]);
    add("first_judges", vec![json!(1), json!(2), json!(3), json!(7), json!(2.5), json!(true), json!(6)]);
    m
}

#[test]
fn load_spec_matches_python() {
    let temp = Temp::new("spec");
    let dir = &temp.0;
    std::fs::create_dir_all(dir.join("real")).unwrap();
    std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
    std::os::unix::fs::symlink("loop2", dir.join("loop1")).unwrap();
    std::os::unix::fs::symlink("loop1", dir.join("loop2")).unwrap();
    let mut r = Rng(0x5eed_0009);
    let mutations = spec_mutations();
    let mut specs: Vec<(String, Value)> = Vec::new();
    for council in [false, true] {
        specs.push(("plain".into(), base_spec(council)));
        for (key, value) in &mutations {
            let mut spec = base_spec(council);
            match value {
                Some(v) => spec[*key] = v.clone(),
                None => {
                    spec.as_object_mut().unwrap().remove(*key);
                }
            }
            specs.push((key.to_string(), spec));
        }
    }
    for _ in 0..1200 {
        let mut spec = base_spec(r.chance(40));
        for _ in 0..r.below(3) + 2 {
            let (key, value) = r.pick(&mutations).clone();
            match value {
                Some(v) => spec[key] = v,
                None => {
                    spec.as_object_mut().unwrap().remove(key);
                }
            }
        }
        specs.push(("random".into(), spec));
    }
    let mut cases = Vec::new();
    for (i, (_, spec)) in specs.iter().enumerate() {
        let place = ["", "real/", "link/", "loop1/"][i % 4];
        let path = dir.join(format!("{place}spec-{i}.json"));
        if std::fs::write(&path, pyjson::dumps(spec)).is_ok() {
            cases.push(json!([path.to_string_lossy()]));
        }
    }
    // File-level failures: missing, a directory, invalid UTF-8, a BOM, CRLF and CR before a JSON error.
    let raw: Vec<(&str, Vec<u8>)> = vec![
        ("bad-start.json", b"{\"a\": \"\xff\"}".to_vec()),
        ("bad-cont.json", b"{\"a\": \"\xe2\x41\"}".to_vec()),
        ("bad-cont2.json", b"{\"a\": \"\xf0\x9f\x41\"}".to_vec()),
        ("bad-end.json", b"{\"a\": \"\xf0\x9f".to_vec()),
        ("bad-end1.json", b"{\"a\": \"\xe2".to_vec()),
        ("bad-sur.json", b"\xed\xa0\x80".to_vec()),
        ("bad-over.json", b"\xc0\x80".to_vec()),
        ("bad-f5.json", b"\xf5\x80\x80\x80".to_vec()),
        ("bad-e0.json", b"\xe0\x80\x80".to_vec()),
        ("bom.json", b"\xef\xbb\xbf{}".to_vec()),
        ("crlf.json", b"{\r\n\"version\": 1,\r\n\"brief\": x\r\n}".to_vec()),
        ("cr.json", b"{\r\"version\": 1,\r\r\"brief\": x\r}".to_vec()),
        ("list.json", b"[]".to_vec()),
        ("deep.json", format!("{}{}", "[".repeat(116_224), "]".repeat(116_224)).into_bytes()),
        ("dup.json", b"{\"version\": 1, \"version\": 1}".to_vec()),
        ("nan.json", b"{\"version\": NaN}".to_vec()),
    ];
    for (name, bytes) in &raw {
        std::fs::write(dir.join(name), bytes).unwrap();
        cases.push(json!([dir.join(name).to_string_lossy()]));
    }
    cases.push(json!([dir.join("missing.json").to_string_lossy()]));
    cases.push(json!([dir.join("real").to_string_lossy()]));
    cases.push(json!([format!("{}//real/../spec-0.json", dir.display())]));
    cases.push(json!([format!("{}/./link/spec-2.json", dir.display())]));
    gate("load_spec", &cases, |c| enc(wf::load_spec(Path::new(s(&c[0])))));
}

/// harness.backend's dict as the Backend the Rust port takes.
fn backend_of(v: &Value) -> Backend {
    let strings = |x: &Value| x.as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect::<Vec<_>>();
    Backend {
        name: s(&v["name"]).into(),
        argv: strings(&v["argv"]),
        style: s(&v["style"]).into(),
        subscription_auth: v.get("subscription_auth").and_then(Value::as_bool),
        lean: v.get("lean").and_then(Value::as_bool),
        tools: v.get("tools").map(strings),
    }
}

#[test]
fn role_backend_matches_python() {
    let mut cases = Vec::new();
    for name in ["codex-chatgpt", "codex", "claude", "claude:foo --model=a --model b", "codex:codex exec --json", "fake", "ollama:x"] {
        for opts in [json!(null), json!({}), json!({"model": "gpt-6-luna", "effort": "low"}), json!({"model": "opus"}), json!({"effort": "high"}),
                     json!({"model": ""}), json!({"effort": ""})] {
            cases.push(json!([name, opts]));
        }
    }
    // Rust takes the backend dict harness.backend returns; get it from Python first.
    let backends = python("role_backend", &cases.iter().map(|c| json!([c[0], null])).collect::<Vec<_>>());
    gate("role_backend", &cases, |c| {
        let i = cases.iter().position(|x| x == c).unwrap();
        let backend = backend_of(&serde_json::from_str::<Value>(&backends[i]).unwrap()["ok"]);
        enc(wf::role_backend(&backend, if c[1].is_null() { None } else { Some(&c[1]) }).map(|b| b.to_value()))
    });
}

// ------------------------------------------------------------------------------------------------------------
// digest, write_json, write_files, spec tests

#[test]
fn digest_and_writing_match_python() {
    let cases = vec![json!([{}]), json!([{"b.py": "x", "a.py": "é😀\n", "A.py": "", "é": "\u{0}\"\\"}]), json!([{"value.py": "def f(x): return 2*x\n"}])];
    gate("digest", &cases, |c| enc(Ok(Value::String(wf::digest(&files(&c[0]))))));

    let temp = Temp::new("write");
    let values = [
        json!({"version": 1, "files": {"é.py": "😀\n"}, "n": [1, 2.5, -0.0, 1e100, 123456789012345678901234567890_u128], "e": {}, "l": [], "t": null}),
        json!([]),
        json!("text"),
        json!({"nested": {"a": [{"b": [1, {"c": "\u{7f}\u{1f}"}]}]}}),
    ];
    let mut cases = Vec::new();
    for (i, v) in values.iter().enumerate() {
        cases.push(json!([temp.0.join(format!("py-{i}.json")).to_string_lossy(), v]));
    }
    cases.push(json!([temp.0.join("missing/x.json").to_string_lossy(), {}]));
    let rust_dir = temp.0.clone();
    gate("write_json", &cases, |c| {
        let path = s(&c[0]).replace("/py-", "/rs-");
        enc(wf::write_json(Path::new(&path), &c[1]).map(|()| Value::String(std::fs::read_to_string(&path).unwrap())))
            .replace(rust_dir.join("rs-").to_string_lossy().as_ref(), rust_dir.join("py-").to_string_lossy().as_ref())
    });

    let sets = [
        json!({"a.py": "x", "d/e/f.md": "é", "d/g.txt": ""}),
        json!({"value.py": "", "check.py": "assert True\n"}),
        json!({"blocked/x": "y"}),
        json!({"a": 5}),
    ];
    let mut cases = Vec::new();
    for (i, f) in sets.iter().enumerate() {
        let py_root = temp.0.join(format!("tree-py-{i}"));
        std::fs::create_dir_all(&py_root).unwrap();
        std::fs::create_dir_all(temp.0.join(format!("tree-rs-{i}"))).unwrap();
        if i == 2 {
            for root in ["py", "rs"] {
                std::fs::write(temp.0.join(format!("tree-{root}-{i}/blocked")), "file").unwrap();
            }
        }
        cases.push(json!([py_root.to_string_lossy(), f]));
    }
    let wanted = python("write_files", &cases);
    for (c, want) in cases.iter().zip(&wanted) {
        let rs_root = s(&c[0]).replace("tree-py-", "tree-rs-");
        let result = wf::write_files(Path::new(&rs_root), &files(&c[1]));
        let got = match result {
            Ok(()) => python("listing", &[json!([rs_root])]).remove(0),
            Err(e) => enc(Err(e)).replace("tree-rs-", "tree-py-"),
        };
        assert_eq!(&got, want, "write_files {}", c[1]);
    }
}

const SPEC_TESTS: &str = "```python\nimport unittest\nfrom value import f\n\n\nclass Brief(unittest.TestCase):\n    def test_doubles_integers(self):\n        \"\"\"\"Return twice x.\" \"\"\"\n        self.assertEqual(f(5), 10)\n\n    def test_keeps_floats(self):\n        \"\"\"\"Return twice x.\" \"\"\"\n        self.assertEqual(f(0.5), 1.0)\n```";

#[test]
fn spec_test_file_and_test_sources_match_python() {
    let q4 = "\"\"\"\"";
    let texts = vec![
        SPEC_TESTS.to_string(),
        format!("```python\nimport unittest\nclass T(unittest.TestCase):\n    def test_a(self):\n        {q4}Say it.{q4}\n        pass\n```"),
        "import unittest\nclass T(unittest.TestCase):\n    def test_x(self): pass\n".into(),
        "def (".into(),
        "class T:\n  def test_a(self):\n pass\n".into(),
        "class T:\n\tdef test_a(self):\n        pass\n".into(),
        "import unittest\n".into(),
        "```py\nclass A:\n    def test_1(self): pass\n```\ntext\n```python extra\nclass B:\n    def test_2(self): pass\n    def test_3(self): pass\n```".into(),
        "class T:\n    def test_a(self): return 1\nclass T:\n    def test_a(self): return 2\n    async def test_b(self): pass\n    def helper(self): pass\ndef test_top(): pass\n".into(),
        "class T:\n    @staticmethod\n    def test_s(): pass\n    class Inner:\n        def test_i(self): pass\n".into(),
        "class Ü:\n    def test_é(self):\n        '😀'\n".into(),
        "x = '\u{0}'\nclass T:\n    def test_a(self): pass\n".into(),
        "```\n```".into(),
        "```python\n\n```".into(),
        String::new(),
        format!("{q4}a{q4} = 1\nclass T:\n    def test_a(self): pass\n"),
        "class T:\n    def test_a(self):\n        x = (\n".into(),
        format!("x = {}1{}\nclass T:\n    def test_a(self): pass\n", "(".repeat(300), ")".repeat(300)),
    ];
    let cases: Vec<Value> = texts.iter().map(|t| json!([t])).collect();
    gate("spec_test_file", &cases, |c| enc(wf::spec_test_file(s(&c[0])).map(|(src, ids)| json!([src, ids]))));
    let cases: Vec<Value> = texts.iter().map(|t| json!([t])).collect();
    gate("test_sources", &cases, |c| {
        enc(wf::test_sources(s(&c[0])).map(|m| Value::Array(m.into_iter().map(|(k, v)| json!([k, v])).collect())))
    });
}

// ------------------------------------------------------------------------------------------------------------
// Corpus sweeps (ignored by default): Ringo's own git history and the Python standard library.

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root()).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8(out.stdout).ok()).flatten()
}

fn stdlib_files(limit: usize) -> Vec<String> {
    let out = Command::new(ringo_cli::python())
        .args(["-I", "-c", "import sysconfig; print(sysconfig.get_paths()['stdlib'])"])
        .output()
        .unwrap();
    let dir = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
    let mut paths = Vec::new();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() && !p.ends_with("site-packages") {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "py" || e == "json" || e == "txt") {
                paths.push(p);
            }
        }
    }
    paths.into_iter().filter_map(|p| std::fs::read_to_string(p).ok()).filter(|t| t.len() < 400_000).take(limit).collect()
}

#[test]
#[ignore]
fn sweep_difflib_and_review_material_over_ringo_history() {
    let log = git(&["log", "--format=%H", "-n", "400", "--", "*.py", "*.md", "*.rs", "*.json"]).unwrap();
    let mut pairs = Vec::new();
    for sha in log.lines() {
        let Some(names) = git(&["diff", "--name-only", &format!("{sha}~1"), sha]) else { continue };
        for name in names.lines() {
            let (Some(before), Some(after)) = (git(&["show", &format!("{sha}~1:{name}")]), git(&["show", &format!("{sha}:{name}")])) else { continue };
            if before.len() + after.len() < 300_000 {
                pairs.push((name.to_string(), before, after));
            }
        }
    }
    let to_lines = |t: &str| -> Vec<String> { ringo_cli::rt::py::splitlines(t, true).into_iter().map(String::from).collect() };
    let mut total = 0;
    for chunk in pairs.chunks(150) {
        let cases: Vec<Value> = chunk.iter().map(|(_, a, b)| json!([to_lines(a), to_lines(b), true])).collect();
        total += gate("matcher", &cases, matcher_rust);
        let cases: Vec<Value> = chunk.iter().map(|(n, a, b)| json!([to_lines(a), to_lines(b), format!("a/{n}"), format!("b/{n}")])).collect();
        total += gate("unified", &cases, |c| enc(Ok(Value::String(difflib::unified_diff(&strs(&c[0]), &strs(&c[1]), s(&c[2]), s(&c[3]))))));
        let cases: Vec<Value> = chunk
            .iter()
            .map(|(n, a, b)| json!([{"brief": "b", "files": {n: a}, "fixtures": {}}, {n: b}, "r", []]))
            .collect();
        total += gate("review_material", &cases, |c| enc(wf::review_material(&c[0], &files(&c[1]), &c[2], &[]).map(Value::String)));
    }
    eprintln!("history sweep: {} file versions, {total} cases identical", pairs.len());
}

#[test]
#[ignore]
fn sweep_numbers_near_miss_and_materialize_over_stdlib() {
    let texts = stdlib_files(3000);
    let mut r = Rng(0x5eed_0010);
    let mut total = 0;
    for chunk in texts.chunks(300) {
        let cases: Vec<Value> = chunk.iter().map(|t| json!([t])).collect();
        total += gate("numbers", &cases, |c| {
            enc(wf::numbers(s(&c[0])).map(|n| Value::Array(n.iter().map(num_row).collect())))
        });
        let mut cases = Vec::new();
        for t in chunk {
            let c: Vec<char> = t.chars().collect();
            if c.len() < 10 {
                continue;
            }
            for _ in 0..3 {
                let a = r.below(c.len() - 5);
                let b = (a + 1 + r.below(80)).min(c.len());
                let mut old: String = c[a..b].iter().collect();
                if r.chance(50) {
                    old.push_str("zq");
                }
                let current = json!({"m.py": t});
                cases.push(json!([current, current, {"edits": [{"path": "m.py", "old": old, "new": "X"}], "rationale": "r"}]));
            }
        }
        total += gate("materialize", &cases, |c| enc(wf::materialize(&files(&c[0]), &files(&c[1]), &c[2]).map(Value::Object)));
        // Large files against edited copies of themselves: popular lines exercise the autojunk heuristic.
        let pairs: Vec<(Vec<String>, Vec<String>)> = chunk
            .iter()
            .step_by(3)
            .map(|t| {
                let a: Vec<String> = ringo_cli::rt::py::splitlines(t, true).into_iter().map(String::from).collect();
                let b = edited(&mut r, &a, 50);
                (a, b)
            })
            .collect();
        let cases: Vec<Value> = pairs.iter().map(|(a, b)| json!([a, b, true])).collect();
        total += gate("matcher", &cases, matcher_rust);
        let cases: Vec<Value> = pairs.iter().map(|(a, b)| json!([a, b, "a/x", "b/x"])).collect();
        total += gate("unified", &cases, |c| enc(Ok(Value::String(difflib::unified_diff(&strs(&c[0]), &strs(&c[1]), s(&c[2]), s(&c[3]))))));
    }
    eprintln!("stdlib sweep: {} files, {total} cases identical", texts.len());
}

#[test]
#[ignore]
fn sweep_strict_json_over_corpus_json_and_mutations() {
    let mut texts: Vec<String> = stdlib_files(100_000).into_iter().filter(|t| t.trim_start().starts_with(['{', '['])).collect();
    let tracked = git(&["ls-files", "*.json"]).unwrap();
    for name in tracked.lines() {
        if let Ok(t) = std::fs::read_to_string(root().join(name))
            && t.len() < 400_000
        {
            texts.push(t);
        }
    }
    let mut r = Rng(0x5eed_0011);
    let mut cases = Vec::new();
    for t in &texts {
        cases.push(json!([t]));
        for _ in 0..20 {
            cases.push(json!([mutate(&mut r, t)]));
        }
    }
    let mut total = 0;
    for chunk in cases.chunks(500) {
        total += gate("strict_json", chunk, strict_rust);
    }
    eprintln!("json sweep: {} documents, {total} cases identical", texts.len());
}


// ------------------------------------------------------------------------------------------------------------
// Phase 2a: checks, bridge, review_findings, outcome, main and run with a fake call function

fn py3() -> String {
    String::from_utf8(
        Command::new(ringo_cli::python()).args(["-I", "-c", "import sys; sys.stdout.write(sys.executable)"]).output().unwrap().stdout,
    )
    .unwrap()
}

fn py_check(name: &str, code: &str, timeout: Value) -> Value {
    json!({"name": name, "argv": [py3(), "-I", "-B", "-c", code], "timeout_s": timeout})
}

/// Runs the same check batches in a py and an rs folder and compares the results, with the folders' paths
/// written alike and seconds as their type.
#[test]
fn checks_match_python() {
    let temp = Temp::new("checks");
    let cases: Vec<(Value, f64)> = vec![
        (json!({"checks": [py_check("ok", "print('hi'); import sys; print('err', file=sys.stderr)", json!(30))]}), 30.0),
        (json!({"checks": [py_check("fail", "import sys; sys.exit(3)", json!(30)), py_check("second", "print(2)", json!(5.5))]}), 30.0),
        (json!({"checks": [py_check("signal", "import os; os.kill(os.getpid(), 9)", json!(30))]}), 30.0),
        (json!({"checks": [py_check("exact", "import sys; sys.stdout.write('x' * 16000)", json!(30))]}), 30.0),
        (json!({"checks": [py_check("over", "import sys; sys.stdout.write('x' * 16001); sys.stderr.write('y' * 40000)", json!(30))]}), 30.0),
        (json!({"checks": [py_check("cut", "import sys; sys.stdout.buffer.write(b'x' * 15999 + 'é'.encode() + b'z')", json!(30))]}), 30.0),
        (json!({"checks": [py_check("bytes", "import sys; sys.stdout.buffer.write(b'a\\xff\\xe2\\x82b\\xf0\\x9f\\x98\\xed\\xa0\\x80c\\xc0\\x80')", json!(30))]}), 30.0),
        (json!({"checks": [py_check("timeout", "import time; print('slow', flush=True); time.sleep(20)", json!(0.5))]}), 30.0),
        (json!({"checks": [py_check("budget", "import time; time.sleep(20)", json!(30))]}), 0.6),
        (json!({"checks": [py_check("env", "import os; print(os.environ.get('PYTHONDONTWRITEBYTECODE'), os.path.basename(os.getcwd()))", json!(30))]}), 30.0),
        (json!({"checks": [py_check("tree", "import subprocess, sys; subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(1.5); open(\"late.txt\", \"w\").write(\"x\")']); print('started')", json!(30))]}), 30.0),
        (json!({"checks": [{"name": "missing", "argv": ["ringo-no-such-command"], "timeout_s": 30}]}), 30.0),
        (json!({"checks": [py_check("never", "print(1)", json!(30))]}), 0.0),
        (json!({"checks": [py_check("never", "print(1)", json!(30))]}), -1.0),
    ];
    let mut py_cases = Vec::new();
    for (i, (spec, remaining)) in cases.iter().enumerate() {
        for side in ["py", "rs"] {
            std::fs::create_dir_all(temp.0.join(format!("{side}-{i}/root"))).unwrap();
            std::fs::create_dir_all(temp.0.join(format!("{side}-{i}/logs"))).unwrap();
        }
        let d = temp.0.join(format!("py-{i}"));
        py_cases.push(json!([spec, d.join("root").to_string_lossy(), d.join("logs").to_string_lossy(), remaining]));
    }
    let want = python("checks", &py_cases);
    for (i, ((spec, remaining), want)) in cases.iter().zip(&want).enumerate() {
        let d = temp.0.join(format!("rs-{i}"));
        let got = wf::checks(spec, &d.join("root"), &d.join("logs"), &|| *remaining).map(|rows| {
            Value::Array(
                rows.into_iter()
                    .map(|mut r| {
                        r["seconds"] = json!(if r["seconds"].as_f64().is_some() { "float" } else { "other" });
                        r
                    })
                    .collect(),
            )
        });
        let got = enc(got).replace(&format!("rs-{i}"), &format!("py-{i}"));
        assert_eq!(&got, want, "checks case {i}: {}", short(&pyjson::dumps(spec)));
    }
    // A check's children are killed with it: neither side's late writer survived to write its file.
    std::thread::sleep(std::time::Duration::from_millis(2000));
    for side in ["py", "rs"] {
        assert!(!temp.0.join(format!("{side}-10/root/late.txt")).exists(), "{side}: a check's child outlived it");
    }
    // the logs are the same bytes
    for i in 0..cases.len() {
        for name in ["check-1.out", "check-1.err", "check-2.out"] {
            let (a, b) = (std::fs::read(temp.0.join(format!("py-{i}/logs/{name}"))), std::fs::read(temp.0.join(format!("rs-{i}/logs/{name}"))));
            assert_eq!(a.ok(), b.ok(), "log {name} of case {i}");
        }
    }
    eprintln!("checks: {} cases identical", cases.len());
}

#[test]
fn bridge_matches_python() {
    let echo = |reply: &str| json!({"council": [py3(), "-I", "-c", reply]});
    let mut cases = Vec::new();
    for program in [
        "import json, sys; print(json.dumps({'version': 1, 'got': sys.stdin.read()}))",
        "print('{\"version\": 1.0, \"x\": \"\\\\u00e9\"}')",
        "print('{\"version\": true}')",
        "print('{\"version\": 2}')",
        "print('[1]')",
        "print('not json')",
        "print('{\"version\": 1, \"version\": 1}')",
        "print('{\"version\": 1, \"x\": NaN}')",
        "import sys; print('partial'); sys.stderr.write('  bad   news \\n' * 50); sys.exit(3)",
        "import sys; sys.exit(1)",
        "",
    ] {
        cases.push(json!([echo(program), {"operation": "prepare", "brief": "é", "samples": 2}, 30]));
    }
    cases.push(json!([{"council": ["ringo-no-such-bridge"]}, {}, 30]));
    gate("bridge", &cases, |c| enc(wf::bridge(&c[0], &c[1], c[2].as_f64().unwrap())));
}

#[test]
fn review_findings_match_python() {
    let cases: Vec<Value> = [
        "What this change does: adds f.\n\nNo defects found.",
        "### 1. Name (`value.py:2`)\n- Problem: rename.\n- Fix: rename.",
        "### 1. A\nx\n### 2. B\ny\n## Open questions\nnone",
        "Looks fine to me.",
        "```\nNo defects found.\n```",
        "Intro\n```\n### 1. inside a fence\n```\nno defects found.",
        "",
        "- no defects found",
        "NO DEFECTS FOUND here",
    ]
    .iter()
    .map(|t| json!([t]))
    .collect();
    gate("review_findings", &cases, |c| enc(Ok(Value::from(wf::review_findings(s(&c[0]))))));
}

#[test]
fn outcome_matches_python() {
    let temp = Temp::new("outcome");
    let out = temp.0.join("run");
    std::fs::create_dir_all(&out).unwrap();
    let usage = json!({"calls": 3, "cost_usd": 0.0, "input": 10, "cache_read": 0, "cache_write": 0, "output": 5, "untokened_calls": 1});
    let cases = vec![
        json!([{"status": "ready_for_review", "final": "/x/final", "rounds": [{}, {}], "usage": usage, "elapsed_s": 12.345678}, out.to_string_lossy()]),
        json!([{"status": "error", "final": null, "rounds": [], "error": "ValueError: x", "usage": {}, "elapsed_s": 0.05}, format!("{}/./", out.display())]),
        json!([{"status": "needs_revision", "final": null, "rounds": [1], "usage": {"calls": 1}, "elapsed_s": 2.25}, temp.0.join("missing/../run").to_string_lossy()]),
        json!([{"status": "x", "final": null, "rounds": [], "usage": {}, "elapsed_s": 3}, "relative-dir"]),
        json!([{"status": "x", "final": null, "usage": {}, "elapsed_s": 1.0}, "r"]),
    ];
    gate("outcome", &cases, |c| enc(wf::outcome(&c[0], Path::new(s(&c[1])))));
}

#[test]
fn main_arguments_and_help_match_python() {
    let temp = Temp::new("main");
    let missing = temp.0.join("missing.json").to_string_lossy().into_owned();
    let inside = root().join("run-inside").to_string_lossy().into_owned();
    let spec = temp.0.join("spec.json");
    std::fs::write(&spec, pyjson::dumps(&base_spec(false))).unwrap();
    let spec = spec.to_string_lossy().into_owned();
    let argvs: Vec<Vec<String>> = [
        vec![],
        vec!["-h"],
        vec!["--help"],
        vec!["--he"],
        vec!["-hx"],
        vec!["-h=x"],
        vec!["--help=x"],
        vec!["--spec"],
        vec!["--spec", "a"],
        vec!["--out", "o"],
        vec!["--spec", "--out", "o"],
        vec!["--spec", "a", "--out"],
        vec!["--spec", "a", "--out", "o", "extra"],
        vec!["x", "--spec", "a", "--out", "o", "y"],
        vec!["--sp", "a", "--o", "o", "--bogus"],
        vec!["--spec=", "--out=o", "-x"],
        vec!["--=x"],
        vec!["--s=a", "--out", "-1"],
        vec!["--spec", "-a b", "--out", "-.5", "--", "z"],
        vec!["-spec", "a"],
        vec!["--spec", "a", "-h", "--out"],
        vec!["--bogus", "-h"],
        vec!["---"],
        vec!["-"],
        vec!["--spec", &missing, "--out", "o"],
        vec!["--spec", &spec, "--out", &inside],
    ]
    .iter()
    .map(|a| a.iter().map(|x| x.to_string()).collect())
    .collect();
    let envs = [
        json!({}),
        json!({"COLUMNS": "40"}),
        json!({"COLUMNS": "120"}),
        json!({"COLUMNS": "1"}),
        json!({"COLUMNS": "25"}),
        json!({"COLUMNS": " 6_0 "}),
        json!({"COLUMNS": "x"}),
        json!({"FORCE_COLOR": "1"}),
        json!({"FORCE_COLOR": "1", "COLUMNS": "30"}),
        json!({"FORCE_COLOR": "1", "NO_COLOR": "1"}),
        json!({"PYTHON_COLORS": "1", "TERM": "dumb"}),
        json!({"PYTHON_COLORS": "0", "FORCE_COLOR": "1"}),
        json!({"FORCE_COLOR": "1", "TERM": "dumb"}),
    ];
    let mut cases = Vec::new();
    for argv in &argvs {
        for env in &envs {
            if argv.len() > 2 && env.as_object().unwrap().len() > 1 {
                continue; // argument errors print the usage only: fewer environments are enough
            }
            cases.push(json!([argv, env]));
        }
    }
    for columns in 1..=130 {
        cases.push(json!([["--help"], {"COLUMNS": columns.to_string()}]));
        cases.push(json!([["--spec"], {"COLUMNS": columns.to_string(), "FORCE_COLOR": "1"}]));
    }
    let want = python_with("main", &cases, &["-B"]);
    let identity = json!({"mode": "checkout", "id": "x", "root": "/r", "quality_claim": null});
    let mut bad = Vec::new();
    for (c, want) in cases.iter().zip(&want) {
        let argv: Vec<String> = c[0].as_array().unwrap().iter().map(|a| s(a).to_string()).collect();
        let env_map = c[1].as_object().unwrap().clone();
        let env = move |k: &str| env_map.get(k).and_then(Value::as_str).map(String::from);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = wf::main_with(&argv, wf::Console { out: &mut out, err: &mut err, stdout_tty: false, terminal_columns: None, env: &env }, None, &identity);
        let got = enc(Ok(json!([code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap()])));
        if &got != want {
            bad.push(format!("{}\n  python: {}\n  rust:   {}", pyjson::dumps(c), short(want), short(&got)));
        }
    }
    assert!(bad.is_empty(), "main: {} of {} differ:\n{}", bad.len(), cases.len(), bad[..bad.len().min(4)].join("\n"));
    eprintln!("main: {} cases identical", cases.len());
}

const FAKE_BRIDGE: &str = "import json, sys
r = json.load(sys.stdin)
if r['operation'] == 'prepare':
    out = {'version': 1, 'fingerprint': 'fixed', 'policy_sha256': 'a'*64,
           'requests': ['JUDGE '+str(n)+' '+json.dumps(json.loads(r['proposal'])['files']) for n in range(r['samples'])]}
else:
    assert len(r['replies']) == r['samples']
    ok = all(json.loads(x)['accept'] for x in r['replies'])
    out = {'version': 1, 'report': {'complete': True, 'scored': [len(r['replies'])],
           'decision': {'action': 'recommend' if ok else 'escalate', 'reasons': []}}}
print(json.dumps(out))
";

const CHECK_PY: &str = "from pathlib import Path\nn={}\nexec(Path('value.py').read_text(), n)\nassert n['f'](5)==10\nassert n['f'](-2)==-4\n";

fn reply(text: &str) -> Value {
    json!({"reply": text, "usage": {"input": 7, "output": 3, "cache_read": 0, "cache_write": 0, "cost_usd": 0.01}})
}

fn proposal(code: &str) -> Value {
    reply(&pyjson::dumps(&json!({"files": {"value.py": code}, "rationale": "Implement it."})))
}

fn accept(yes: bool) -> Value {
    reply(&pyjson::dumps(&json!({"accept": yes})))
}

fn raise(kind: &str, msg: &str, data: Value) -> Value {
    json!({"raise": [kind, msg, data]})
}

/// The error a fake call function raises, as the driver's make_error builds it.
fn make_error(kind: &str, msg: &str, data: &Value) -> Error {
    use ringo_cli::rt::{backends, harness, Exc};
    match kind {
        "AdmissionRefused" => harness::admission_refused(msg),
        "BudgetExceeded" => harness::budget_exceeded(msg),
        "CallError" => harness::call_error(msg, data.get("usage").cloned(), data.get("raw").and_then(Value::as_str).map(String::from)),
        "JudgeError" => backends::judge_error(msg, false),
        "Stopped" => backends::stopped(&["x".to_string()], 1.0),
        "TimeoutExpired" => backends::timeout_expired(&["x".to_string()], 1.0),
        "KeyError" => Error::new(Exc::Key, ringo_cli::rt::py::repr(msg)),
        _ => Error::value(msg),
    }
}

/// The driver's scripted call function in Rust: the replies in order (a probe answers OK without using one), an
/// item's `soft` runs the watch's soft-limit check with the call already ended, `sleep` waits first, `raise` fails.
fn fake_call(replies: Vec<Value>, watch: bool, seen: std::sync::Arc<std::sync::Mutex<Vec<Value>>>) -> CallFn {
    let next = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    CallFn::new(watch, move |backend: &Backend, system: &str, message: &str, _timeout: Num, _model: Option<&str>, w: Option<std::sync::Arc<dyn ringo_cli::rt::backends::Watch>>| {
        seen.lock().unwrap().push(json!([backend.to_value(), ringo_cli::rt::harness::sha(system), message]));
        if message == ringo_cli::rt::harness::PROBE_MESSAGE {
            return Ok(("OK".to_string(), Some(json!({"input": 1, "output": 1, "cache_read": 0, "cache_write": 0}))));
        }
        let Some(item) = replies.get(next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)) else {
            // Python's next() on the exhausted replies: StopIteration, which run() does not catch
            return Err(Error::new(ringo_cli::rt::Exc::StopIteration, ""));
        };
        if item.get("soft") == Some(&json!(true))
            && let Some(w) = &w
        {
            w.set_done();
            let snapshot: ringo_cli::rt::backends::Snapshot =
                std::sync::Arc::new(|| Ok(("{\"type\": \"turn.started\"}\nnot json\n".to_string(), "stream error: reconnecting\n".to_string())));
            w.soft(1.5, snapshot);
        }
        if let Some(t) = item.get("sleep").and_then(Value::as_f64) {
            std::thread::sleep(std::time::Duration::from_secs_f64(t));
        }
        if let Some(r) = item.get("raise") {
            return Err(make_error(s(&r[0]), s(&r[1]), &r[2]));
        }
        Ok((s(&item["reply"]).to_string(), Some(item.get("usage").cloned().unwrap_or(json!({})))))
    })
}

/// Timing values (seconds, elapsed, state times, pid) as a marker, and the rs folder's paths as the py folder's.
fn normalize(v: &mut Value, from: &str, to: &str) {
    match v {
        Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                if ["seconds", "elapsed_s", "started", "updated", "pid"].contains(&k.as_str()) && x.is_number() {
                    *x = json!("<n>");
                } else if k == "message_sha256" {
                    *x = json!("<sha of a message the calls already compare, timing included>");
                } else {
                    normalize(x, from, to);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| normalize(x, from, to)),
        Value::String(t) => {
            // a JSON-protocol writer message carries the check results, timing included
            static SECONDS: std::sync::LazyLock<regex::Regex> =
                std::sync::LazyLock::new(|| regex::Regex::new(r#""(seconds|elapsed_s)": [0-9.e+-]+"#).unwrap());
            *t = SECONDS.replace_all(&t.replace(from, to), "\"$1\": \"<n>\"").into_owned();
        }
        _ => {}
    }
}

/// Every file under dir: relative path and content, JSON files normalized.
fn tree(dir: &Path, from: &str, to: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                out.push((rel, "/".into()));
                stack.push(p);
                continue;
            }
            let text = String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned();
            let text = if rel.ends_with(".json") || rel.ends_with(".jsonl") {
                text.lines()
                    .map(|line| match serde_json::from_str::<Value>(line).or_else(|_| serde_json::from_str::<Value>(&text)) {
                        Ok(mut v) => {
                            normalize(&mut v, from, to);
                            pyjson::dumps(&v)
                        }
                        Err(_) => line.replace(from, to),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                text.replace(from, to)
            };
            out.push((rel, text));
        }
    }
    out.sort();
    out
}

struct RunCase {
    name: &'static str,
    spec: Value,
    replies: Vec<Value>,
    /// The call function takes a watch (so run() builds harness's Watch and probe for each call).
    watch: bool,
}

fn council_spec(dir: &Path) -> Value {
    std::fs::write(dir.join("bridge.py"), FAKE_BRIDGE).unwrap();
    json!({"version": 1, "brief": "Implement f(x)=2*x.\n\nHard constraints:\n- Return twice x.\n", "files": {"value.py": ""},
           "fixtures": {"check.py": CHECK_PY}, "checks": [{"name": "reference", "argv": ["{python}", "-I", "-B", "check.py"]}],
           "writer": "fake", "judge": "fake", "allow_tool_backends": true, "allow_execution": true,
           "council": [py3(), dir.join("bridge.py").to_string_lossy()], "rubric": "unused.json",
           "max_rounds": 3, "max_seconds": 60, "samples": 2})
}

fn with(spec: &Value, changes: Value) -> Value {
    let mut s = spec.clone();
    for (k, v) in changes.as_object().unwrap() {
        if v.is_null() {
            s.as_object_mut().unwrap().remove(k);
        } else {
            s[k.as_str()] = v.clone();
        }
    }
    s
}

const SPEC_TESTS_REPLY: &str = "```python\nimport unittest\nfrom value import f\n\n\nclass Brief(unittest.TestCase):\n    def test_doubles_integers(self):\n        self.assertEqual(f(5), 10)\n\n    def test_keeps_floats(self):\n        self.assertEqual(f(0.5), 1.0)\n        self.assertIsInstance(f(0.5), float)\n```";

const THREE_SPEC_TESTS: &str = "```python\nimport unittest\nfrom value import f\n\n\nclass Brief(unittest.TestCase):\n    def test_ints(self):\n        self.assertEqual(f(5), 10)\n\n    def test_floats(self):\n        self.assertIsInstance(f(0.5), float)\n\n    def test_negative(self):\n        self.assertEqual(f(-2), -4)\n```";

fn run_cases(dir: &Path) -> Vec<RunCase> {
    let base = council_spec(dir);
    let text = with(&base, json!({"protocol": "text"}));
    let good = "def f(x): return 2*x\n";
    let plain_json = with(&base, json!({"council": null, "rubric": null, "samples": null}));
    let plain = json!({"version": 1, "brief": "Implement f(x).\n\nReturn twice x.\n", "files": {"value.py": ""},
        "fixtures": {"check.py": "from value import f\nassert f(5) == 10\n"}, "checks": [{"name": "reference", "argv": ["{python}", "-B", "check.py"]}],
        "writer": "fake", "judge": "fake", "allow_tool_backends": true, "allow_execution": true, "max_rounds": 3, "max_seconds": 60,
        "protocol": "text", "spec_tests": {"python": "{python}"}});
    vec![
        RunCase { watch: false, name: "ready", spec: base.clone(), replies: vec![proposal(good), accept(true), accept(true)] },
        RunCase { watch: false, name: "revise", spec: base.clone(), replies: vec![proposal("def f(x): return x\n"), proposal(good), accept(true), accept(true)] },
        RunCase { watch: false, name: "malformed", spec: base.clone(), replies: vec![reply("not json"), proposal("# keep\ndef f(x): return x\n"),
            reply(&pyjson::dumps(&json!({"edits": [{"path": "value.py", "old": "return 2*x", "new": "return x+x"}], "rationale": "r"}))),
            reply(&pyjson::dumps(&json!({"edits": [{"path": "value.py", "old": "return x", "new": "return 2*x"}], "rationale": "r"})))] },
        RunCase { watch: false, name: "escalate", spec: with(&base, json!({"samples": 4, "first_judges": 2})), replies: vec![proposal(good), accept(true), accept(false), accept(true), accept(true),
            proposal("def f(x): return x + x\n"), accept(true), accept(true)] },
        RunCase { watch: false, name: "exhausted", spec: base.clone(), replies: vec![proposal(good), accept(true)] },
        RunCase { watch: false, name: "repeated", spec: base.clone(), replies: vec![proposal("")] },
        RunCase { watch: false, name: "deep", spec: base.clone(), replies: vec![reply(&format!("{}0{}", "[".repeat(200_000), "]".repeat(200_000))), proposal(good), accept(true), accept(true)] },
        RunCase { watch: false, name: "admission", spec: base.clone(), replies: vec![raise("AdmissionRefused", "no room", json!({}))] },
        RunCase { watch: false, name: "call-error", spec: base.clone(), replies: vec![raise("CallError", "bad reply", json!({"usage": {"input": 4, "output": 0, "cache_read": 0, "cache_write": 0}, "raw": "events"}))] },
        RunCase { watch: false, name: "stopped-retry", spec: base.clone(), replies: vec![raise("Stopped", "", json!({})), proposal(good), accept(true), accept(true)] },
        RunCase { watch: false, name: "stopped-twice", spec: base.clone(), replies: vec![raise("Stopped", "", json!({})), raise("Stopped", "", json!({}))] },
        RunCase { watch: false, name: "judge-error", spec: base.clone(), replies: vec![proposal(good), raise("JudgeError", "the judge failed", json!({}))] },
        RunCase { watch: false, name: "key-error", spec: base.clone(), replies: vec![raise("KeyError", "k", json!({}))] },
        RunCase { watch: false, name: "mutation", spec: base.clone(), replies: vec![proposal("from pathlib import Path\nPath('check.py').write_text('pass')\ndef f(x): return 2*x\n")] },
        RunCase { watch: false, name: "crlf", spec: base.clone(), replies: vec![proposal("def f(x):\r\n    return 2*x\r\n")] },
        RunCase { watch: false, name: "timeout", spec: with(&base, json!({"checks": [{"name": "slow", "argv": ["{python}", "-c", "import time; time.sleep(5)"], "timeout_s": 0.3}], "max_rounds": 1})),
            replies: vec![proposal(good)] },
        RunCase { watch: false, name: "all-fail", spec: with(&base, json!({"max_rounds": 2})), replies: vec![proposal("def f(x): return x\n"), proposal("def f(x): return 3*x\n")] },
        RunCase { watch: false, name: "budget", spec: with(&base, json!({"max_seconds": 0.5, "checks": [{"name": "slow", "argv": ["{python}", "-c", "import time; time.sleep(3)"], "timeout_s": 10}]})),
            replies: vec![proposal(good)] },
        RunCase { watch: false, name: "repair", spec: with(&text, json!({"protocol_repair": true})), replies: vec![reply("@@@ REPLACE value.py\nnot there\n@@@ WITH\nx\n@@@ END\n"),
            reply("@@@ FILE value.py\ndef f(x):\n    return 2*x\n"), accept(true), accept(true)] },
        RunCase { watch: false, name: "text", spec: text.clone(), replies: vec![reply("@@@ RATIONALE\nwhy\n@@@ FILE value.py\ndef f(x):\n    return x\n"),
            reply("@@@ REPLACE value.py\n    return x\n@@@ WITH\n    return 2*x\n@@@ END\n"), accept(false), accept(true),
            reply("@@@ RATIONALE\nThe judges are wrong.\n"), accept(true), accept(true)] },
        RunCase { watch: false, name: "no-regression", spec: with(&text, json!({"no_regression": true})), replies: vec![reply("@@@ FILE value.py\ndef f(x):\n    return 2*x\n"), accept(false), accept(false),
            reply("@@@ FILE value.py\ndef f(x):\n    return x\n"), reply("@@@ FILE value.py\ndef f(x):\n    return x*2\n"), accept(true), accept(true)] },
        RunCase { watch: false, name: "unpriced", spec: with(&base, json!({"max_cost": 0.015, "unpriced_call_usd": 0.004})), replies: vec![proposal(good), accept(true), accept(true), proposal("x"), accept(true)] },
        RunCase { watch: false, name: "doc-script", spec: with(&text, json!({"files": {"REPORT.md": ""}, "fixtures": {"data.csv": "a,b\n1,2\n3,4\n", "check.py": "assert 'total' in open('REPORT.md').read()\n"},
            "checks": [{"name": "c", "argv": ["{python}", "-B", "check.py"]}], "scripts": {"python": "{python}"}, "no_regression": true})),
            replies: vec![reply("@@@ SCRIPT\nimport csv\nprint('total', sum(int(r['b']) for r in csv.DictReader(open('data.csv'))))\n@@@ FILE REPORT.md\ntotal: 6\n"), accept(false), accept(true),
                reply("@@@ REPLACE REPORT.md\ntotal: 6\n@@@ WITH\ntotal: 9\n@@@ END\n"), reply("@@@ RATIONALE\nThe script shows 6.\n"), accept(true), accept(true)] },
        RunCase { watch: false, name: "spec-tests", spec: plain.clone(), replies: vec![reply(SPEC_TESTS_REPLY), reply("@@@ FILE value.py\ndef f(x):\n    return int(2*x)\n"),
            reply("@@@ FILE value.py\ndef f(x):\n    return x\n"), reply("@@@ DISPUTE Brief.test_keeps_floats\n\"Return twice x.\"\n@@@ FILE value.py\ndef f(x):\n    return 3*x\n")] },
        RunCase { watch: false, name: "candidates", spec: with(&plain, json!({"candidates": 2, "max_cost": 10, "unpriced_call_usd": 0.0})),
            replies: vec![reply(THREE_SPEC_TESTS), reply("@@@ FILE value.py\ndef f(x):\n    return int(2*x)\n"), reply("@@@ FILE value.py\ndef f(x):\n    return abs(2*x)\n"),
                reply("@@@ FILE value.py\ndef f(x):\n    return x\n"), reply("@@@ FILE value.py\ndef f(x):\n    return -x\n")] },
        RunCase { watch: false, name: "candidates-fail", spec: with(&plain, json!({"candidates": 3, "max_cost": 10, "unpriced_call_usd": 0.0, "max_rounds": 2, "spec_tests": null})),
            replies: vec![reply("@@@ FILE value.py\ndef f(x):\n    return x\n"), reply("not blocks"), reply("@@@ FILE value.py\ndef f(x):\n    return x\n"),
                reply("@@@ FILE value.py\ndef f(x):\n    return 3*x\n")] },
        RunCase { watch: false, name: "script-only", spec: with(&plain, json!({"files": {"REPORT.md": ""}, "fixtures": {"check.py": "assert 'total' in open('REPORT.md').read()\n"},
            "spec_tests": null, "scripts": {"python": "{python}"}, "candidates": 2, "max_cost": 10, "unpriced_call_usd": 0.0})),
            replies: vec![reply("@@@ SCRIPT\nprint('total', 6)\n"), reply("@@@ SCRIPT\nprint('total', 7)\n"), reply("@@@ FILE REPORT.md\nno word\n"), reply("@@@ FILE REPORT.md\nstill none\n")] },
        RunCase { watch: false, name: "plain-ready", spec: plain_json.clone(), replies: vec![proposal(good), reply("What this change does: f.\n\nNo defects found.")] },
        RunCase { watch: false, name: "plain-findings", spec: plain_json.clone(), replies: vec![proposal(good), reply("### 1. Name (`value.py:1`)\n- Problem: rename.\n- Fix: rename."),
            proposal("def f(x): return x + x\n")] },
        RunCase { watch: false, name: "plain-findings-then-fail", spec: plain_json.clone(), replies: vec![proposal(good), reply("Unreadable review."),
            proposal("def f(x): return x\n"), proposal("def f(x): return 2 * x\n")] },
        RunCase { watch: false, name: "review-retry", spec: plain_json.clone(), replies: vec![proposal(good),
            raise("CallError", "the reviewer failed", json!({"usage": {"input": 2, "output": 0, "cache_read": 0, "cache_write": 0}, "raw": "partial"})),
            reply("No defects found.")] },
        RunCase { watch: false, name: "review-bounded-refused", spec: with(&plain_json, json!({"max_seconds": 1.5})), replies: vec![proposal(good),
            json!({"raise": ["JudgeError", "slow and failed", {}], "sleep": 1.6})] },
        RunCase { watch: false, name: "review-doc", spec: with(&plain, json!({"files": {"REPORT.md": ""}, "fixtures": {"check.py": "assert 'total' in open('REPORT.md').read()\n"},
            "checks": [{"name": "c", "argv": ["{python}", "-B", "check.py"]}], "spec_tests": null, "scripts": {"python": "{python}"}})),
            replies: vec![reply("@@@ SCRIPT\nprint('total', 6)\n@@@ FILE REPORT.md\ntotal: 6\n"), reply("No defects found.")] },
        RunCase { watch: false, name: "spec-tests-broken", spec: plain.clone(), replies: vec![reply("```python\nfrom value import g\nimport unittest\nclass T(unittest.TestCase):\n    def test_x(self):\n        pass\n```"),
            reply("@@@ FILE value.py\ndef f(x):\n    return 2*x\n"), reply("No defects found.")] },
        RunCase { watch: true, name: "watch-int", spec: with(&base, json!({"timeout_s": 120, "max_seconds": 1000})), replies: vec![proposal(good), accept(true), accept(true)] },
        RunCase { watch: true, name: "watch-float", spec: with(&base, json!({"timeout_s": 2.5, "max_seconds": 1000})), replies: vec![proposal(good), accept(true), accept(true)] },
        RunCase { watch: true, name: "watch-soft-probe", spec: with(&plain_json, json!({"timeout_s": 120, "max_seconds": 1000})),
            replies: vec![json!({"reply": proposal(good)["reply"], "soft": true}), reply("No defects found.")] },
        RunCase { watch: true, name: "watch-soft-cost-cap", spec: with(&base, json!({"timeout_s": 120, "max_seconds": 1000, "max_cost": 5, "unpriced_call_usd": 0.0})),
            replies: vec![json!({"reply": proposal(good)["reply"], "soft": true}), accept(true), accept(true)] },
        RunCase { watch: true, name: "watch-stopped", spec: with(&base, json!({"timeout_s": 120, "max_seconds": 1000})),
            replies: vec![raise("Stopped", "", json!({})), proposal(good), accept(true), accept(true)] },
        RunCase { watch: false, name: "spec-tests-none", spec: with(&plain, json!({"max_rounds": 2})), replies: vec![reply("no tests here"), reply("@@@ FILE value.py\ndef f(x):\n    return x\n"),
            reply("@@@ FILE value.py\ndef f(x):\n    return x + 1\n")] },
    ]
}

/// run() against workflow.run with the same scripted call function: the report, the calls it made (backend,
/// system prompt hash, message) and every file it wrote must match, timing aside.
#[test]
fn run_matches_python_with_a_fake_call_function() {
    let temp = Temp::new("run");
    let identity = serde_json::from_str::<Value>(&python("identity", &[json!([])])[0]).unwrap()["ok"].clone();
    let cases = run_cases(&temp.0);
    let mut py_cases = Vec::new();
    for c in &cases {
        let path = temp.0.join(format!("{}.json", c.name));
        std::fs::write(&path, pyjson::dumps(&c.spec)).unwrap();
        let mode = if c.watch { "watch" } else { "plain" };
        py_cases.push(json!([path.to_string_lossy(), temp.0.join(format!("py-{}", c.name)).to_string_lossy(), c.replies, mode]));
    }
    let want = python("run", &py_cases);
    for (c, want) in cases.iter().zip(&want) {
        let (py_out, rs_out) = (temp.0.join(format!("py-{}", c.name)), temp.0.join(format!("rs-{}", c.name)));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let call = fake_call(c.replies.clone(), c.watch, std::sync::Arc::clone(&seen));
        let spec_path = temp.0.join(format!("{}.json", c.name));
        let result = match wf::run(&spec_path, &rs_out, Some(&call), &identity) {
            Ok(report) => json!({"report": report}),
            Err(e) => json!({"raised": [e.type_name(), e.msg]}),
        };
        let mut result = result;
        result["seen"] = Value::Array(seen.lock().unwrap().clone());
        let (from, to) = (rs_out.to_string_lossy().into_owned(), py_out.to_string_lossy().into_owned());
        let (from_real, to_real) = (
            ringo_cli::rt::py::realpath(&rs_out).unwrap().to_string_lossy().into_owned(),
            ringo_cli::rt::py::realpath(&py_out).unwrap().to_string_lossy().into_owned(),
        );
        normalize(&mut result, &from_real, &to_real);
        normalize(&mut result, &from, &to);
        let mut want: Value = serde_json::from_str(want).unwrap();
        normalize(&mut want, "\u{0}", "\u{0}");
        let (got, wanted) = (pyjson::dumps(&result), pyjson::dumps(&want["ok"]));
        if got != wanted {
            let at = got.chars().zip(wanted.chars()).take_while(|(a, b)| a == b).count();
            let around = |t: &str| t.chars().skip(at.saturating_sub(200)).take(500).collect::<String>();
            panic!("run case {}: differs at {at}\n  python: {}\n  rust:   {}", c.name, around(&wanted), around(&got));
        }
        let status = want["ok"].get("report").map_or_else(|| want["ok"]["raised"].clone(), |r| r["status"].clone());
        eprintln!("run case {}: {} ({} calls)", c.name, status, want["ok"]["seen"].as_array().map_or(0, Vec::len));
        let (a, b) = (tree(&py_out, "\u{0}", "\u{0}"), tree(&rs_out, &from_real, &to_real));
        let names = |t: &[(String, String)]| t.iter().map(|x| x.0.clone()).collect::<Vec<_>>();
        assert_eq!(names(&a), names(&b), "run case {}: files", c.name);
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.1, y.1, "run case {}: {}", c.name, x.0);
        }
    }
    eprintln!("run: {} cases identical (report, calls and files)", cases.len());
}

/// Ctrl-C during a check: both runtimes kill the check's process tree, write the report and state as interrupted
/// and exit (Python by its KeyboardInterrupt, the binary with 130, as harness::task does).
#[test]
fn work_interrupted_by_sigint_matches_python() {
    let temp = Temp::new("sigint");
    std::fs::write(temp.0.join("writer.py"), "import json, sys\nsys.stdin.read()\nprint(json.dumps({'files': {'value.py': 'x = 1\\n'}, 'rationale': 'r'}))\n").unwrap();
    let spec = json!({"version": 1, "brief": "b", "files": {"value.py": ""}, "fixtures": {},
        "checks": [{"name": "slow", "argv": ["{python}", "-c",
            "import subprocess, sys, time; subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(3); open(\"late\", \"w\").write(\"x\")']); time.sleep(30)"]}],
        "writer": format!("{} {}", py3(), temp.0.join("writer.py").display()), "judge": "fake", "allow_tool_backends": true,
        "allow_execution": true, "max_rounds": 1, "max_seconds": 1000});
    let spec_path = temp.0.join("spec.json");
    std::fs::write(&spec_path, pyjson::dumps(&spec)).unwrap();
    let state = temp.0.join("release-state");
    let mut outcomes = Vec::new();
    for arm in ["python", "native"] {
        let out = temp.0.join(arm);
        let mut cmd = if arm == "python" {
            let mut c = Command::new(py3());
            c.arg(root().join("ringo.py"));
            c
        } else {
            Command::new(env!("CARGO_BIN_EXE_ringo"))
        };
        cmd.args(["work", "--spec"]).arg(&spec_path).arg("--out").arg(&out);
        cmd.env("RINGO_HOME", root()).env("RINGO_RELEASE_HOME", &state).env_remove("RINGO_CHILD").env_remove("RINGO_PINNED");
        let child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let log = out.join("round-1/check-1.out");
        let started = std::time::Instant::now();
        while !log.exists() && started.elapsed().as_secs() < 30 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
        unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
        let done = child.wait_with_output().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3500)); // past the check child's write
        assert!(!out.join("round-1/candidate/late").exists(), "{arm}: the check's child outlived the interrupt");
        let stderr = String::from_utf8_lossy(&done.stderr).into_owned();
        assert!(stderr.trim_end().ends_with("KeyboardInterrupt"), "{arm}: {stderr}");
        outcomes.push((out, done.status));
    }
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(outcomes[0].1.signal().map(|s| 128 + s).or(outcomes[0].1.code()), Some(130), "python's exit");
    assert_eq!(outcomes[1].1.code(), Some(130), "the binary's exit");
    let (py_out, rs_out) = (outcomes[0].0.clone(), outcomes[1].0.clone());
    let (from, to) = (
        ringo_cli::rt::py::realpath(&rs_out).unwrap().to_string_lossy().into_owned(),
        ringo_cli::rt::py::realpath(&py_out).unwrap().to_string_lossy().into_owned(),
    );
    let (a, b) = (tree(&py_out, "\u{0}", "\u{0}"), tree(&rs_out, &from, &to));
    let skip = |name: &str| name.contains("implementation") || name == "source.json";
    for ((na, ta), (nb, tb)) in a.iter().zip(&b) {
        assert_eq!(na, nb, "interrupted runs wrote different files");
        if !skip(na) && na != "report.json" {
            assert_eq!(ta, tb, "interrupted run: {na}");
        }
    }
    let report = |t: &[(String, String)]| -> Value {
        let text = &t.iter().find(|x| x.0 == "report.json").unwrap().1;
        let mut r: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap(); // tree() wrote it normalized
        r.as_object_mut().unwrap().remove("implementation"); // which runtime ran is the one intended difference
        r
    };
    assert_eq!(report(&a), report(&b), "interrupted run: report.json");
    assert_eq!(report(&b)["status"], "interrupted");
}
