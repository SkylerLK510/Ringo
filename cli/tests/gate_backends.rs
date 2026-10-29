//! Differential gate: `ringo_cli::rt::backends` against `backends.py`, offline. The JSON scanner is compared on
//! generated replies; run_isolated runs fake `codex`/`claude` programs (a /bin/sh wrapper that records the exact
//! environment, then a Python script that reports argv, stdin and the private homes, prints canned output, sleeps,
//! refreshes its login or ignores SIGTERM) under a temporary fake owner home, once through Python and once through
//! Rust (this test binary re-run as a driver), and every observable is compared. Never touches the real ~/.codex.
#![cfg(unix)]

use ringo_cli::rt::backends::{self, Decoder, Isolated, JsonFail, Snapshot, Watch};
use ringo_cli::rt::pyjson;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn python_exe() -> &'static str {
    static EXE: OnceLock<String> = OnceLock::new();
    EXE.get_or_init(|| {
        let out = Command::new("python3").args(["-c", "import sys; print(sys.executable)"]).output().expect("python3");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    })
}

/// Runs Python code with the worktree root on sys.path, input on stdin; returns stdout.
fn python(code: &str, input: &str) -> String {
    let prelude = format!("import sys; sys.path.insert(0, {})\n", pyjson::quote(&root().to_string_lossy()));
    let mut child = Command::new(python_exe())
        .args(["-B", "-c", &(prelude + code)])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_string();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
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
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const PIECES: &[&str] = &[
    // the Python test's pieces
    "{", "}", "{\"a\": 1}", "{\"a\": {\"b\": [1, 2]}}", "\"", "\\\"", " ", "\n", "prose ", ":", ",", "[", "]",
    "{ \"k\" : \"v\" }", "{}", "{\"x\": NaN}", "{\"x\": 1e999}", "{\"a\": 1, \"a\": 2}", "{\"a\": \"{\"}", "```json\n",
    "{\"q1\": \"c\", \"q2\": true}", "{'single': 1}", "{ a: 1 }", "{\"t\": 1,}", "\t{\r\"n\": null}",
    // more of the scanner's paths
    "{\"a\": -1.5e3, \"b\": [true, false, null]}", "{\"u\": \"\\u00e9\\ud83d\\ude00\\/\\b\\f\\n\\r\\t\"}", "{\"x\": Infinity}",
    "{\"x\": -Infinity}", "{\"x\": -1e400}", "{\"x\": 1e-400}", "{\"x\": 12345678901234567890123}", "{\"e\": \"\\x\"}",
    "{\"c\": \"\u{1}\"}", "{\"k\": 1 \"j\": 2}", "{\"k\" 1}", "{\"k\": [1, 2,]}", "{\"k\": [1 2]}", "{\"a\":",
    "{\"a\": {\"b\": {\"c\": {}}}}", "é", "😀", "\r\n", "{\"\\\"\": \"}\"}", "{\"x\": -}", "{\"x\": 01}", "{\"x\": 1.}",
    "{\"x\": 1e}", "{\"x\": 1e+}", "{\"x\": .5}", "{\"x\": nul}", "{\"x\": tru}", "{\"x\": fals}", "{\"x\": \"\\u12\"}",
    "{\"x\": \"\\ud83d\\ude0\"}", "{\"x\": -0}", "{\"x\": -0.0}", "{\"x\": 1E+2}", "{\"x\": 2.5E-3}", "{  }", "{\n}",
    "{\"dup\": 1, \"dup\": {\"dup\": 2, \"dup\": 3}}", "[{\"in\": \"array\"}]", "{\"a\": \"line\nbreak\"}",
    "{\"n\": [[[[]]], {}, [{}]]}", "{\"a\": [1, {\"b\": NaN}]}", "{\"a\" : 1 , \"b\" : [ ] }", "{\"a\": [1,\n]}",
    "{\"x\": 0.1, \"y\": 1e16, \"z\": 1e-7, \"w\": 123.456}", "{\"€\": \"é\"}", "{\"x\": \"\\\\\"}", "\\u007b", "{\"x\": I}",
    "{\"x\": -I}", "{\"x\": Na}", "{\"x\": [", "{\"x\": {\"y\":", "{\"x\": \"abc", "{\"x\": \"a\\", "{\"x\": \"\\u",
    "{\"x\": \"\\u0041", "{\"x\": \"\\ud83d\\ude00", "{\"x\": \"\\ud83d\\ude0", "{\"\\u00e9\": 1}",
];

const SOUP: &[char] = &[
    '{', '}', '[', ']', '"', ':', ',', '\\', ' ', '\n', '\t', 'a', '1', '0', 'e', 'E', '-', '+', '.', 'n', 'u', 'l',
    't', 'r', 'f', 's', 'N', 'I', 'é', '😀', '{', '"', '}',
];

fn random_value(rng: &mut Rng, depth: usize) -> String {
    let ws = |rng: &mut Rng| [" ", "", "\n", "\t", "  ", "\r\n"][rng.below(6)].to_string();
    match rng.below(if depth > 3 { 6 } else { 9 }) {
        0 => ["null", "true", "false"][rng.below(3)].into(),
        1 => ["0", "-1", "42", "3.25", "-0.5e-3", "1E10", "123456789012345678901234567890", "-0"][rng.below(8)].into(),
        2 => pyjson::quote(["", "a", "é", "{}", "\"q\"", "line\nbreak", "\\", "😀", "tab\t"][rng.below(9)]),
        3 => ["\"\\u00e9\"", "\"\\ud83d\\ude00\"", "\"\\/\"", "\"a\\\"b\""][rng.below(4)].into(),
        4 | 5 => pyjson::quote(["k", "key", "é", ""][rng.below(4)]),
        6 => {
            let n = rng.below(4);
            let items: Vec<String> = (0..n).map(|_| format!("{}{}{}", ws(rng), random_value(rng, depth + 1), ws(rng))).collect();
            format!("[{}]", items.join(","))
        }
        _ => {
            let n = rng.below(4);
            let items: Vec<String> = (0..n)
                .map(|_| {
                    let k = pyjson::quote(["a", "b", "a", "é", "x y", ""][rng.below(6)]);
                    format!("{}{k}{}:{}{}{}", ws(rng), ws(rng), ws(rng), random_value(rng, depth + 1), ws(rng))
                })
                .collect();
            format!("{{{}}}", items.join(","))
        }
    }
}

/// Whether text holds a \\uD800-\\uDFFF escape that decodes to an unpaired surrogate (conservatively: escaped
/// backslashes are not told apart). Those strings are the reported U+FFFD deviation, tested on their own; a high
/// surrogate followed by a malformed \\u escape is an error in both and stays in.
fn lone_surrogate_escape(text: &str) -> bool {
    let c: Vec<char> = text.chars().collect();
    let escape = |i: usize| i + 2 <= c.len() && c[i] == '\\' && c[i + 1] == 'u';
    let unit = |i: usize| -> Option<u32> {
        (escape(i) && i + 6 <= c.len()).then(|| c[i + 2..i + 6].iter().collect::<String>())
            .filter(|h| h.chars().all(|x| x.is_ascii_hexdigit()))
            .and_then(|h| u32::from_str_radix(&h, 16).ok())
    };
    let mut i = 0;
    while i < c.len() {
        match unit(i) {
            Some(0xd800..=0xdbff) => match unit(i + 6) {
                Some(0xdc00..=0xdfff) => i += 12,
                None if escape(i + 6) => i += 6, // the next escape is malformed: an error, not a surrogate
                _ => return true,
            },
            Some(0xdc00..=0xdfff) => return true,
            _ => i += 1,
        }
    }
    false
}

fn corpus() -> Vec<String> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut texts = Vec::new();
    for _ in 0..6000 {
        let n = 1 + rng.below(40);
        texts.push((0..n).map(|_| *rng.pick(PIECES)).collect());
    }
    for _ in 0..3000 {
        let n = 1 + rng.below(60);
        texts.push((0..n).map(|_| *rng.pick(SOUP)).collect());
    }
    for _ in 0..3000 {
        let mut chars: Vec<char> = format!("{{\"top\": {}}}", random_value(&mut rng, 0)).chars().collect();
        match rng.below(4) {
            0 => {}
            1 => chars.truncate(rng.below(chars.len() + 1)),
            2 => {
                let at = rng.below(chars.len());
                chars.remove(at);
            }
            _ => {
                let at = rng.below(chars.len() + 1);
                chars.insert(at, *rng.pick(SOUP));
            }
        }
        let wrapped: String = chars.into_iter().collect();
        texts.push(match rng.below(3) {
            0 => wrapped,
            1 => format!("Here is the answer:\n```json\n{wrapped}\n```\n"),
            _ => format!("{wrapped} and {{\"second\": {}}}", random_value(&mut rng, 1)),
        });
    }
    texts.retain(|t| !lone_surrogate_escape(t));
    texts
}

const TOP_LEVEL_PY: &str = r#"
import json, backends
out = []
for t in json.load(sys.stdin):
    try:
        out.append({'ok': [[json.dumps(o), o.repeats_a_key] for o in backends.top_level_objects(t)]})
    except Exception as e:
        out.append({'err': [type(e).__name__, str(e)]})
print(json.dumps(out))
"#;

fn top_level_rust(text: &str) -> Value {
    match backends::top_level_objects(text) {
        Ok(objs) => json!({"ok": objs.into_iter().map(|o| json!([pyjson::dumps(&Value::Object(o.map)), o.repeats_a_key])).collect::<Vec<_>>()}),
        Err(e) => json!({"err": [e.exc.name(), e.msg]}),
    }
}

/// Compares top_level_objects on every text; returns how many texts yielded objects and raised.
fn compare_top_level(texts: &[String]) -> (usize, usize) {
    let input = pyjson::dumps(&json!(texts));
    let py: Vec<Value> = serde_json::from_str(&python(TOP_LEVEL_PY, &input)).unwrap();
    assert_eq!(py.len(), texts.len());
    let (mut found, mut raised) = (0, 0);
    for (text, want) in texts.iter().zip(&py) {
        let got = top_level_rust(text);
        assert_eq!(&got, want, "top_level_objects({})", pyjson::quote(&text.chars().take(300).collect::<String>()));
        found += usize::from(want["ok"].as_array().is_some_and(|a| !a.is_empty()));
        raised += usize::from(want.get("err").is_some());
    }
    (found, raised)
}

#[test]
fn top_level_objects_matches_python_on_generated_replies() {
    let texts = corpus();
    let (found, raised) = compare_top_level(&texts);
    eprintln!("top_level_objects: {} texts, {found} with objects, {raised} raising", texts.len());
    assert!(found > 3000, "the corpus must exercise successes: {found}");
}

#[test]
fn top_level_objects_matches_python_at_the_budget_bounds() {
    let mut texts = Vec::new();
    // FAILED_STARTS: each snippet passes the start filter and fails; 1000 failures are allowed, the 1001st ends it
    for n in [0, 1, 998, 999, 1000, 1001, 1002, 1500] {
        let snippets: String = (0..n).map(|i| format!("see {{\"k{i}\": 1,}} and ")).collect();
        texts.push(snippets + "{\"q1\": \"a\"}");
    }
    // FAILED_PARSE: unterminated objects each charged to the end of the text, the answer last
    for m in (100..=140).chain([0, 1, 50, 200, 400]) {
        texts.push("{\"a\": ".repeat(m) + "{\"answer\": 1}");
    }
    // refused numbers are charged only the distance parsed
    texts.push("{\"x\": NaN}\n".repeat(70) + &" ".repeat(100_000) + "{\"answer\": 1}");
    texts.push("{\"a\": {\"b\": 1e999}, \"c\": {\"d\": 1}}".into());
    // the hostile replies of the Python test
    texts.push(("x".repeat(99) + "{").repeat(10_000));
    texts.push("{\"a\": ".repeat(33_000));
    texts.push("{\"a\": ".repeat(170_000));
    texts.push("{\"".repeat(250_000));
    texts.push(("x".repeat(99) + "{").repeat(5_000) + "{\"answer\": 1}");
    // too deep for either to parse, and as deep as Rust keeps (MAX_DEPTH)
    texts.push("{\"a\":".repeat(120_000) + "1" + &"}".repeat(120_000));
    texts.push("{\"a\":".repeat(backends::MAX_DEPTH) + "1" + &"}".repeat(backends::MAX_DEPTH));
    texts.push("{\"a\": ".to_string() + &"[".repeat(backends::MAX_DEPTH - 1) + &"]".repeat(backends::MAX_DEPTH - 1) + "}");
    // int() refuses more than 4300 digits (Python raises out of the generator); floats have no such limit
    for digits in [4300, 4301, 5000] {
        texts.push(format!("{{\"n\": {}}}", "7".repeat(digits)));
        texts.push(format!("{{\"n\": -{}}}", "7".repeat(digits)));
        texts.push(format!("prose {{\"n\": {}", "7".repeat(digits)));
    }
    texts.push(format!("{{\"f\": {}.5}}", "7".repeat(5000)));
    texts.push(format!("{{\"f\": 1{}e-5000}}", "0".repeat(5000)));
    for text in &texts {
        let started = Instant::now();
        let _ = backends::top_level_objects(text);
        assert!(started.elapsed() < Duration::from_secs(4), "slow on a {}-char text", text.len());
    }
    let (found, raised) = compare_top_level(&texts);
    eprintln!("budget bounds: {} texts, {found} with objects, {raised} raising", texts.len());
}

#[test]
fn reported_deviation_objects_nested_deeper_than_rust_keeps() {
    // Python 3.14 parses about 116,000 levels (stack-bound); Rust stops at MAX_DEPTH, because deeper serde_json
    // values overflow the stack when dropped or written.
    let deep = |d: usize| "{\"a\":".repeat(d) + "1" + &"}".repeat(d);
    let texts = [deep(backends::MAX_DEPTH + 1), deep(20_000)];
    let py: Vec<Value> = serde_json::from_str(&python(TOP_LEVEL_PY, &pyjson::dumps(&json!(texts)))).unwrap();
    let outer = |v: &Value| v["ok"][0][0].as_str().map(|t| t.len());
    assert_eq!(outer(&py[0]), Some(deep(backends::MAX_DEPTH + 1).len() + backends::MAX_DEPTH + 1)); // the whole object
    assert_eq!(outer(&top_level_rust(&texts[0])), Some(deep(backends::MAX_DEPTH).len() + backends::MAX_DEPTH)); // the inner one
    assert_eq!(outer(&py[1]), Some(deep(20_000).len() + 20_000));
    assert_eq!(top_level_rust(&texts[1]), json!({"ok": []}));
}

#[test]
fn unpaired_surrogate_escapes_become_replacement_characters() {
    // A Rust string cannot hold a lone surrogate: Python yields '\ud800', Rust U+FFFD (a reported deviation).
    let text = "{\"s\": \"\\ud800\", \"t\": \"\\udc00x\", \"u\": \"\\ud800\\u0041\", \"v\": \"\\ud800\\ud800\\udc00\"}";
    let py: Vec<Value> = serde_json::from_str(&python(TOP_LEVEL_PY, &pyjson::dumps(&json!([text])))).unwrap();
    let want = "{\"s\": \"~ud800\", \"t\": \"~udc00x\", \"u\": \"~ud800A\", \"v\": \"~ud800~ud800~udc00\"}".replace('~', "\\");
    assert_eq!(py[0], json!({"ok": [[want, false]]}));
    let got = "{\"s\": \"~ufffd\", \"t\": \"~ufffdx\", \"u\": \"~ufffdA\", \"v\": \"~ufffd~ud800~udc00\"}".replace('~', "\\");
    assert_eq!(top_level_rust(text), json!({"ok": [[got, false]]}));
}

const LOADS_PY: &str = r#"
import json, backends
out = []
for t in json.load(sys.stdin):
    try:
        v = json.loads(t, parse_constant=backends._no_constant, parse_float=backends._finite_float)
        out.append({'ok': json.dumps(v)})
    except RecursionError:
        out.append({'recursion': True})
    except ValueError as e:
        out.append({'err': str(e)})
print(json.dumps(out))
"#;

/// Compares `Decoder::strict().loads` with json.loads(parse_constant=_no_constant, parse_float=_finite_float) on
/// every text: values, and the exact error texts; returns how many parsed and how many failed with a text.
fn compare_loads(texts: &[String]) -> (usize, usize) {
    let py: Vec<Value> = serde_json::from_str(&python(LOADS_PY, &pyjson::dumps(&json!(texts)))).unwrap();
    let decoder = Decoder::strict();
    let (mut ok, mut errs) = (0, 0);
    for (text, want) in texts.iter().zip(&py) {
        let got = match decoder.loads(text) {
            Ok(v) => json!({"ok": pyjson::dumps(&v)}),
            Err(JsonFail::Recursion { .. }) => json!({"recursion": true}),
            Err(e) => json!({"err": e.into_error(text).msg}),
        };
        assert_eq!(&got, want, "json.loads({})", pyjson::quote(&text.chars().take(200).collect::<String>()));
        ok += usize::from(want.get("ok").is_some());
        errs += usize::from(want.get("err").is_some());
    }
    (ok, errs)
}

#[test]
fn strict_loads_matches_python_values_and_error_texts() {
    let mut texts = corpus();
    texts.retain(|t| t.len() < 400);
    texts.truncate(6000);
    for t in [
        "", " ", "1", " -0 ", "\"a\"", "[1, 2]", "-", "NaN", "Infinity", "-Infinity", "1e400", "-1e400", "[1,]", "{\"a\":1,}",
        "nul", "\u{feff}{}", "{} x", "{}\n\n x", "[1] [2]", "\"\\ud83d\\ude00\"", "1.", "1e5", "1E-5", "0.0000001", "[\n\n  ,]",
        "\"unterminated", "\"\\ud800\\uzzzz\"", "\"\\ud800\\u00\"", "{\"a\" 1}", "{1: 2}",
        // the \\u bounds: four digits that end the text, and a pair whose second escape ends it
        "\"\\u0041", "\"\\u004", "\"\\ud800\\udc00", "\"\\ud800\\udc0", "\"\\ud800\\u", "\"\\ud800\\", "\"\\ud800\\udzzz",
        "\"\\ud800\\udzzzx", "\"\\ud800\\udc00x", "[\"\\u0041", "{\"\\ud83d\\ude00", "[\"a\" \"b\"]", "\"\\q\"", "\"\\u00zz\"", "\"\u{7}\"", "tru", "[true,false,null]",
    ] {
        texts.push(t.to_string());
    }
    texts.push("[".repeat(200_000));
    texts.push("[".repeat(backends::MAX_DEPTH) + &"]".repeat(backends::MAX_DEPTH));
    let (ok, errs) = compare_loads(&texts);
    eprintln!("strict loads: {} texts, {ok} values, {errs} error texts", texts.len());
    assert_eq!(backends::no_constant("NaN").unwrap_err().msg, "NaN is not a JSON number");
    assert_eq!(backends::finite_float("1e400").unwrap_err().msg, "1e400 is not a finite number");
    assert_eq!(backends::finite_float("-2.5e3").unwrap(), -2500.0);
}

fn utf8_files(dir: &Path, exts: &[&str], out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() && !p.is_symlink() {
            utf8_files(&p, exts, out);
        } else if p.extension().is_some_and(|x| exts.iter().any(|e| x == *e))
            && let Ok(text) = fs::read_to_string(&p)
        {
            out.push(text);
        }
    }
}

/// The corpus sweep: every UTF-8 file of this repository, its git history (each commit's message and patch) and
/// the Python standard library's .py, .json and .txt files, whole and line by line.
#[test]
#[ignore]
fn corpus_sweep_matches_python() {
    let mut texts = Vec::new();
    let git = |args: &[&str]| String::from_utf8_lossy(&Command::new("git").args(args).current_dir(root()).output().unwrap().stdout).into_owned();
    for name in git(&["ls-files"]).lines() {
        if let Ok(text) = fs::read_to_string(root().join(name)) {
            texts.push(text);
        }
    }
    let files = texts.len();
    let history = git(&["log", "-p", "--format=@@COMMIT@@%n%B"]);
    texts.extend(history.split("@@COMMIT@@\n").filter(|c| !c.is_empty()).map(String::from));
    let commits = texts.len() - files;
    let stdlib = python("import sysconfig; print(sysconfig.get_paths()['stdlib'])", "");
    utf8_files(Path::new(stdlib.trim()), &["py", "json", "txt"], &mut texts);
    let skipped = texts.iter().filter(|t| lone_surrogate_escape(t)).count();
    texts.retain(|t| !lone_surrogate_escape(t));
    let (mut found, mut raised) = (0, 0);
    for chunk in texts.chunks(400) {
        let (f, r) = compare_top_level(chunk);
        found += f;
        raised += r;
    }
    let mut lines: Vec<String> = texts
        .iter()
        .flat_map(|t| t.lines())
        .map(str::trim)
        .filter(|l| l.len() < 2000 && (l.starts_with('{') || l.starts_with('[') || l.starts_with('"')))
        .map(String::from)
        .collect();
    lines.sort();
    lines.dedup();
    let (mut ok, mut errs) = (0, 0);
    for chunk in lines.chunks(20_000) {
        let (o, e) = compare_loads(chunk);
        ok += o;
        errs += e;
    }
    eprintln!(
        "corpus sweep: {} texts ({files} repository files, {commits} commits, {} stdlib files; {skipped} skipped for \
         unpaired surrogate escapes): {found} with objects, {raised} raising; strict loads on {} lines: {ok} values, \
         {errs} error texts",
        texts.len() + skipped,
        texts.len() + skipped - files - commits,
        lines.len()
    );
}

const RAW_DECODE_PY: &str = r#"
import json
decoder = json.JSONDecoder(parse_constant=lambda name: 'C' + name, parse_float=lambda text: 'F' + text)
out = []
for t in json.load(sys.stdin):
    try:
        v, end = decoder.raw_decode(t, 0)
        out.append({'ok': [json.dumps(v), end]})
    except json.JSONDecodeError as e:
        out.append({'err': [e.msg, e.pos]})
    except ValueError as e:
        out.append({'raised': str(e)})
print(json.dumps(out))
"#;

/// Every string up to `max` characters over `alphabet` (with `prefix` in front).
fn every(prefix: &str, alphabet: &[char], max: usize) -> Vec<String> {
    let mut all = vec![prefix.to_string()];
    let mut layer = vec![prefix.to_string()];
    for _ in 0..max {
        layer = layer.iter().flat_map(|p| alphabet.iter().map(move |c| format!("{p}{c}"))).collect();
        all.extend(layer.iter().cloned());
    }
    all.retain(|t| !t.is_empty() && !lone_surrogate_escape(t));
    all
}

#[test]
fn scanner_matches_python_on_every_short_input() {
    let mut texts = every("", &['0', '1', '-', '+', '.', 'e', 'E', 'x'], 6); // numbers
    texts.extend(every("\"", &['"', '\\', 'u', '0', 'd', '8', 'c', 'n', 'x'], 6)); // strings and escapes
    texts.extend(every("", &['[', ']', '{', '}', '"', 'a', ':', ',', ' ', '1'], 5)); // containers and delimiters
    for word in ["null", "true", "false", "NaN", "Infinity", "-Infinity"] {
        for cut in 0..=word.len() {
            for tail in ["", "x", " ", "]", "1", "e"] {
                texts.push(format!("{}{tail}", &word[..cut]));
                texts.push(format!("[{}{tail}", &word[..cut]));
            }
        }
    }
    let constant = |name: &str| Ok(json!(format!("C{name}")));
    let float = |text: &str| Ok(json!(format!("F{text}")));
    let decoder = Decoder { parse_constant: &constant, parse_float: &float, object_pairs_hook: &backends::dict, max_depth: backends::MAX_DEPTH };
    let mut compared = 0;
    for chunk in texts.chunks(100_000) {
        let py: Vec<Value> = serde_json::from_str(&python(RAW_DECODE_PY, &pyjson::dumps(&json!(chunk)))).unwrap();
        for (text, want) in chunk.iter().zip(&py) {
            let chars: Vec<char> = text.chars().collect();
            let got = match decoder.raw_decode(&chars, 0) {
                Ok((v, end)) => json!({"ok": [pyjson::dumps(&v), end]}),
                Err(JsonFail::Decode { msg, pos }) => json!({"err": [msg, pos]}),
                Err(JsonFail::Raised(e)) => json!({"raised": e.msg}),
                Err(JsonFail::Recursion { .. }) => json!("recursion"),
            };
            assert_eq!(&got, want, "raw_decode({})", pyjson::quote(text));
            compared += 1;
        }
    }
    eprintln!("raw_decode: {compared} short inputs");
}

#[test]
fn presets_and_constants_match_python() {
    let py = python(
        "import json, backends\nprint(json.dumps([list(backends.JUDGE_PRESETS.items()), backends.RAW_PEEK, \
         backends.CODEX_INSTRUCTIONS, list(backends.CATALOG_TOOL_FIELDS), backends.OBJECT_START.pattern, \
         backends.FAILED_PARSE, backends.FAILED_STARTS]))",
        "",
    );
    let rust = json!([
        backends::JUDGE_PRESETS.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
        backends::RAW_PEEK,
        backends::CODEX_INSTRUCTIONS,
        backends::CATALOG_TOOL_FIELDS,
        backends::OBJECT_START,
        backends::FAILED_PARSE,
        backends::FAILED_STARTS
    ]);
    assert_eq!(serde_json::from_str::<Value>(&py).unwrap(), rust);
    assert_eq!(backends::judge_preset("codex"), Some(backends::JUDGE_PRESETS[1].1));
    assert_eq!(backends::judge_preset("grok"), None);
}

// ---------- run_isolated with fake programs ----------

/// Records the exact environment (a Python fake would add __CF_USER_TEXT_ENCODING on macOS), then runs the fake.
const WRAPPER: &str = "#!/bin/sh\nif [ -n \"$FAKE_REPORT\" ]; then\n  if [ \"$1\" = debug ]; then /usr/bin/env -0 > \"$FAKE_REPORT.models.env\"; \
else /usr/bin/env -0 > \"$FAKE_REPORT.env\"; fi\nfi\nexec {python} -B {fake} \"$@\"\n";

const FAKE_PY: &str = r#"
import json, os, signal, stat, subprocess, sys, time
E = os.environ.get
args = sys.argv[1:]

def tree(root):
    if not root or not os.path.isdir(root):
        return None
    out = {'.': oct(stat.S_IMODE(os.lstat(root).st_mode))}
    for dirpath, dirnames, filenames in os.walk(root):
        for name in sorted(dirnames + filenames):
            p = os.path.join(dirpath, name)
            st = os.lstat(p)
            entry = {'mode': oct(stat.S_IMODE(st.st_mode))}
            if stat.S_ISLNK(st.st_mode):
                entry['link'] = os.readlink(p)
            elif stat.S_ISDIR(st.st_mode):
                entry['dir'] = True
            else:
                entry['data'] = open(p, 'rb').read().decode('utf-8', 'replace')
            out[os.path.relpath(p, root)] = entry
    return out

OK = {'models': [{'slug': 'a', 'tool_mode': 'code_mode_only', 'multi_agent_version': 'v2',
                  'multi_agent_reasoning_effort': 'xhigh', 'apply_patch_tool_type': 'freeform',
                  'experimental_supported_tools': ['clock'], 'context_window': 9}, {'slug': 'b'}]}
CATALOGS = {
    'ok': json.dumps(OK).encode(),
    'garbage': b'{"models": [',
    'dict-empty': b'{"models": {}}', 'dict': b'{"models": {"a": 1}}',
    'str-empty': b'{"models": ""}', 'str': b'{"models": "ab"}',
    'list-of-list': b'{"models": [{"slug": "a", "tool_mode": "x"}, [1]]}', 'list-of-str': b'{"models": ["a"]}',
    'num': b'{"models": 3}', 'none': b'{"models": null}', 'nomodels': b'{"other": []}', 'toplist': b'[{"models": []}]',
    'inf': b'{"models": [{"slug": "a", "x": Infinity, "y": -Infinity, "z": 1e999, "tool_mode": "m"}]}',
    'big': ('{"z": 1, "models": [{"tool_mode": 1, "experimental_supported_tools": null, "n": 123456789012345678901234567890, '
            '"f": [1.0, 1e5, 1E-7, -0.0, -0, 0.1, 2.5e300, 1e16], "u": "\u00e9\U0001F600", "e": "\\u00e9\\n\\"\\ud83d\\ude00", '
            '"d": {}, "dup": 1, "dup": 2, "apply_patch_tool_type": "x"}, {}], "z": 3}').encode(),
    'utf16': json.dumps(OK).encode('utf-16'), 'utf16le': json.dumps(OK).encode('utf-16-le'),
    'utf16be': json.dumps(OK).encode('utf-16-be'), 'utf32': json.dumps(OK).encode('utf-32'),
    'utf32le': json.dumps(OK).encode('utf-32-le'), 'bom': b'\xef\xbb\xbf' + json.dumps(OK).encode(),
    'bom2': b'\xef\xbb\xbf\xef\xbb\xbf' + json.dumps(OK).encode(), 'latin1': '{"models": [{"s": "\u00e9"}]}'.encode('latin-1'),
    'bigint': b'{"models": [], "n": ' + b'1' * 5000 + b'}', 'trailing': b'{"models": []} x', 'ws': b' \n{"models": []}\n ',
    'empty': b'', 'nan': b'{"models": [{"slug": "a", "v": NaN}]}', 'surrogate': b'{"models": [{"slug": "\\ud800"}]}',
    'deep': b'{"models": [' + b'[' * 200000 + b']' * 200000 + b']}',
}

if args[:2] == ['debug', 'models']:
    with open(E('FAKE_REPORT') + '.models', 'w') as f:
        json.dump({'argv': args, 'cwd': os.getcwd(), 'home': E('HOME'), 'codex_home': E('CODEX_HOME'),
                   'stdin': sys.stdin.read() if sys.stdin else None}, f)
    mode = E('FAKE_CATALOG', 'ok')
    if mode == 'fail':
        sys.exit(2)
    if mode == 'sleep':
        time.sleep(5)
        mode = 'ok'
    sys.stdout.buffer.write(CATALOGS[mode])
    sys.exit(0)

files = {}
for k, a in enumerate(args):
    if a == '-c' and k + 1 < len(args) and '=' in args[k + 1]:
        key, value = args[k + 1].split('=', 1)
        if key in ('model_instructions_file', 'model_catalog_json'):
            files[key] = open(json.loads(value), encoding='utf-8').read()
tmp_root = os.path.dirname(os.getcwd())
report = {'argv': args, 'stdin': sys.stdin.buffer.read().decode('utf-8', 'replace'), 'cwd': os.getcwd(),
          'cwd_entries': sorted(os.listdir()), 'files': files, 'pid_is_session_leader': os.getsid(0) == os.getpid(),
          'tmp_root': {n: oct(stat.S_IMODE(os.lstat(os.path.join(tmp_root, n)).st_mode)) for n in sorted(os.listdir(tmp_root))},
          'codex_home': tree(E('CODEX_HOME')) if E('FAKE_CODEX') else None,
          'home': tree(E('HOME')) if E('FAKE_CODEX') else None}
if E('FAKE_REPORT'):
    with open(E('FAKE_REPORT'), 'w') as f:
        json.dump(report, f, sort_keys=True)

if E('FAKE_TERM_IGNORE'):
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
refresh = E('FAKE_REFRESH')
if refresh:
    c = E('CODEX_HOME')
    a = os.path.join(c, 'auth.json')
    if refresh == 'fifo':
        os.unlink(a)
        os.mkfifo(a)
    elif refresh.startswith('symlink:'):
        os.unlink(a)
        os.symlink(refresh[len('symlink:'):], a)
    else:
        body = {'garbage': b'{"tokens": ', 'utf16': json.dumps({'tokens': {'refresh_token': 'r16'}}).encode('utf-16'),
                'nan': b'{"tokens": {"refresh_token": "rn"}, "x": NaN}',
                'deep': b'{"x": ' + b'[' * 200000 + b']' * 200000 + b'}',
                'deep5k': b'{"tokens": {"refresh_token": "rd"}, "x": ' + b'[' * 5000 + b']' * 5000 + b'}'}.get(refresh) or json.dumps({'tokens': {'refresh_token': refresh}}).encode()
        with open(os.path.join(c, 'tmp'), 'wb') as f:
            f.write(body)
        os.replace(os.path.join(c, 'tmp'), a)
if E('FAKE_OWNER_WRITE'):
    with open(E('FAKE_OWNER_WRITE'), 'w') as f:
        f.write(json.dumps({'tokens': {'refresh_token': 'owner'}}))
if E('FAKE_CACHE_EDIT'):
    with open(os.path.join(E('CODEX_HOME'), 'models_cache.json'), 'w') as f:
        f.write('{"edited": true}')
if E('FAKE_LOCKDOWN'):
    for base in (E('HOME'), E('CODEX_HOME')):
        sub = os.path.join(base, 'ro')
        os.mkdir(sub)
        with open(os.path.join(sub, 'f'), 'w') as f:
            f.write('x')
        os.chmod(sub, 0)
        os.chmod(base, 0o500)
if E('FAKE_TICKS'):
    ticker = f'import time\nfor _ in range(400):\n    open({E("FAKE_TICKS")!r}, "a").write("x")\n    time.sleep(0.05)\n'
    subprocess.Popen([sys.executable, '-c', ticker])
    for _ in range(500):  # the grandchild is running before the fake goes on
        if os.path.exists(E('FAKE_TICKS')):
            break
        time.sleep(0.01)
mode = E('FAKE_STDOUT', 'events')
if mode == 'events':
    for event in [{'type': 'thread.started', 'thread_id': 't'}, {'type': 'turn.started'},
                  {'type': 'item.completed', 'item': {'type': 'agent_message', 'text': 'r\u00e9ponse'}},
                  {'type': 'turn.completed', 'usage': {'input_tokens': 10, 'output_tokens': 2}}]:
        print(json.dumps(event))
elif mode == 'claude':
    print(json.dumps({'type': 'result', 'is_error': False, 'result': 'ok', 'usage': {'input_tokens': 3}}))
elif mode == 'invalid':
    sys.stdout.buffer.write(b'ok \xff\xfe \xed\xa0\x80 \xe2\x82 \xf0\x9f\x98 \xc0\xaf end\r\n\xe2\x82')
elif mode == 'flood':
    sys.stdout.buffer.write(b'a' * 999_999 + '\u00e9'.encode() + b'tail' * 10)
sys.stdout.flush()
err = E('FAKE_STDERR')
if err == 'long':
    sys.stderr.write('warn  \t\n ' * 60 + 'tail \u00e9\u00e9 end \xff')
elif err == 'invalid':
    sys.stderr.buffer.write(b'bad \xff\xfe bytes')
elif err:
    sys.stderr.write(err)
sys.stderr.flush()
if E('FAKE_SLEEP'):
    time.sleep(float(E('FAKE_SLEEP')))
if E('FAKE_KILLSELF'):
    os.kill(os.getpid(), signal.SIGKILL)
sys.exit(int(E('FAKE_EXIT', '0')))
"#;

const DRIVER_PY: &str = r#"
import json, os, threading, backends
req = json.load(open(os.environ['RINGO_GATE_REQUEST'], encoding='utf-8'))
class W:
    def __init__(self, limit_s, stop_after_soft):
        self.limit_s, self.stop_after_soft = limit_s, stop_after_soft
        self.stop, self.done, self.calls = threading.Event(), threading.Event(), []
    def soft(self, elapsed, snapshot):
        out, err = snapshot()
        call = {'elapsed_ok': elapsed >= req['timeout_s'], 'stdout': out, 'stderr': err}
        self.calls.append(call)
        if self.stop_after_soft:
            self.stop.set()
        call['done_seen'] = self.done.wait(30)
w = W(**req['watch']) if req['watch'] else None
res = {}
try:
    res['ok'] = backends.run_isolated(req['argv'], req['prompt'], req['timeout_s'], watch=w, **req['kw'])
except BaseException as e:
    res['exc'] = 'OSError' if isinstance(e, OSError) else type(e).__name__
    res['msg'] = str(e)
    for k in ('stdout', 'limit', 'fatal'):
        if hasattr(e, k):
            res[k] = getattr(e, k)
if w:
    res['soft'] = w.calls
with open(req['result'], 'w', encoding='utf-8') as f:
    json.dump(res, f)
"#;

struct GateWatch {
    limit_s: f64,
    timeout_s: f64,
    stop_after_soft: bool,
    stop: AtomicBool,
    done: Mutex<bool>,
    wake: Condvar,
    calls: Mutex<Vec<Value>>,
}

impl Watch for GateWatch {
    fn limit_s(&self) -> f64 {
        self.limit_s
    }
    fn stop_is_set(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
    fn set_done(&self) {
        *self.done.lock().unwrap() = true;
        self.wake.notify_all();
    }
    fn soft(&self, elapsed: f64, snapshot: Snapshot) {
        let (out, err) = snapshot().unwrap();
        let mut call = json!({"elapsed_ok": elapsed >= self.timeout_s, "stdout": out, "stderr": err});
        if self.stop_after_soft {
            self.stop.store(true, Ordering::SeqCst);
        }
        let done = self.done.lock().unwrap();
        let (done, _) = self.wake.wait_timeout_while(done, Duration::from_secs(30), |d| !*d).unwrap();
        call["done_seen"] = json!(*done);
        self.calls.lock().unwrap().push(call);
    }
}

/// The Rust side of each run_isolated case: this test binary re-run with the case's environment.
#[test]
fn driver() {
    let Some(path) = std::env::var_os("RINGO_GATE_REQUEST") else { return };
    let req: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    let argv: Vec<String> = req["argv"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect();
    let timeout_s = req["timeout_s"].as_f64().unwrap();
    let watch = req["watch"].as_object().map(|w| {
        Arc::new(GateWatch {
            limit_s: w["limit_s"].as_f64().unwrap(),
            timeout_s,
            stop_after_soft: w["stop_after_soft"].as_bool().unwrap(),
            stop: AtomicBool::new(false),
            done: Mutex::new(false),
            wake: Condvar::new(),
            calls: Mutex::new(Vec::new()),
        })
    });
    let flag = |k: &str| req["kw"].get(k).and_then(Value::as_bool).unwrap_or(false);
    let kw = Isolated {
        subscription_auth: flag("subscription_auth"),
        codex_home: flag("codex_home"),
        lean: flag("lean"),
        watch: watch.clone().map(|w| w as Arc<dyn Watch>),
    };
    let mut res = serde_json::Map::new();
    match backends::run_isolated(&argv, req["prompt"].as_str().unwrap(), timeout_s, kw) {
        Ok(out) => {
            res.insert("ok".into(), json!(out));
        }
        Err(e) => {
            res.insert("exc".into(), json!(e.exc.name()));
            res.insert("msg".into(), json!(e.msg));
            for k in ["stdout", "limit", "fatal"] {
                if let Some(v) = e.data.as_ref().and_then(|d| d.get(k)) {
                    res.insert(k.into(), v.clone());
                }
            }
        }
    }
    if let Some(w) = watch {
        res.insert("soft".into(), json!(*w.calls.lock().unwrap()));
    }
    fs::write(req["result"].as_str().unwrap(), pyjson::dumps(&Value::Object(res))).unwrap();
}

fn scratch(name: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ringo-gate-backends-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let dir = fs::canonicalize(&base).unwrap().join(name);
    unlock_remove(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Removes a tree a fake may have made read-only.
fn unlock_remove(path: &Path) {
    if fs::symlink_metadata(path).is_err() {
        return;
    }
    let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(path).status();
    let _ = fs::remove_dir_all(path);
}

/// Replaces each random tempfile name with X's, so runs compare.
fn norm(text: &str) -> String {
    let mut out = text.to_string();
    for marker in ["council-judge-", ".ringo-auth-"] {
        let mut from = 0;
        while let Some(i) = out[from..].find(marker) {
            let at = from + i + marker.len();
            let end = out[at..].char_indices().take(8).last().map_or(at, |(j, c)| at + j + c.len_utf8());
            if out[at..end].len() == 8 && out[at..end].bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
                out.replace_range(at..end, "XXXXXXXX");
            }
            from = at;
        }
    }
    out
}

/// The seconds in "timed out after N seconds", replaced by N.
fn split_seconds(msg: &str) -> (String, Option<f64>) {
    let key = "' timed out after ";
    let Some(i) = msg.rfind(key) else { return (msg.to_string(), None) };
    let rest = &msg[i + key.len()..];
    let n = rest.split(' ').next().unwrap();
    (format!("{}{key}N{}", &msg[..i], &rest[n.len()..]), n.parse().ok())
}

/// A file tree as data: modes, contents, symlink targets.
fn tree(root: &Path) -> Value {
    let mut out = serde_json::Map::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let meta = fs::symlink_metadata(&p).unwrap();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let mode = format!("{:o}", meta.permissions().mode() & 0o7777);
            let entry = if meta.file_type().is_symlink() {
                json!({"mode": mode, "link": fs::read_link(&p).unwrap()})
            } else if meta.is_dir() {
                stack.push(p.clone());
                json!({"mode": mode, "dir": true})
            } else {
                json!({"mode": mode, "data": String::from_utf8_lossy(&fs::read(&p).unwrap_or_default())})
            };
            out.insert(norm(&rel), entry);
        }
    }
    let mut sorted: Vec<_> = out.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Object(sorted.into_iter().collect())
}

fn env_dump(path: &Path) -> Value {
    let Ok(raw) = fs::read(path) else { return Value::Null };
    let mut pairs: Vec<(String, String)> = raw
        .split(|&b| b == 0)
        .filter(|e| !e.is_empty())
        .map(|e| {
            let text = String::from_utf8_lossy(e);
            let (k, v) = text.split_once('=').unwrap();
            (k.to_string(), norm(v))
        })
        .collect();
    pairs.sort();
    json!(pairs)
}

fn read_json(path: &Path) -> Value {
    fs::read_to_string(path).ok().map_or(Value::Null, |t| serde_json::from_str(&norm(&t)).unwrap())
}

#[derive(Debug, PartialEq)]
struct Seen {
    result: Value,
    seconds: Option<f64>,
    report: Value,
    env: Value,
    models: Value,
    models_env: Value,
    owner: Value,
    stderr: String,
    tmp_left: Value,
    ticks_stopped: Option<bool>,
}

#[derive(Clone, Copy)]
enum Lang {
    Python,
    Rust,
}

struct Case {
    name: &'static str,
    argv: Vec<String>,
    prompt: String,
    timeout_s: f64,
    kw: Value,
    watch: Value,
    env: Vec<(String, String)>,
    owner: fn(&Path),
}

impl Case {
    fn new(name: &'static str, argv: &[&str]) -> Case {
        Case {
            name,
            argv: argv.iter().map(|a| a.to_string()).collect(),
            prompt: "Review this.\r\nré 😀\n".into(),
            timeout_s: 20.0,
            kw: json!({}),
            watch: Value::Null,
            env: Vec::new(),
            owner: plain_owner,
        }
    }
    fn kw(mut self, kw: Value) -> Case {
        self.kw = kw;
        self
    }
    fn env(mut self, k: &str, v: &str) -> Case {
        self.env.push((k.into(), v.into()));
        self
    }
    fn timeout(mut self, s: f64) -> Case {
        self.timeout_s = s;
        self
    }
    fn watch(mut self, limit_s: f64, stop_after_soft: bool) -> Case {
        self.watch = json!({"limit_s": limit_s, "stop_after_soft": stop_after_soft});
        self
    }
    fn owner(mut self, f: fn(&Path)) -> Case {
        self.owner = f;
        self
    }
    fn prompt(mut self, p: &str) -> Case {
        self.prompt = p.into();
        self
    }
}

fn plain_owner(home: &Path) {
    fs::create_dir_all(home.join(".agents/skills/ringo-harness")).unwrap();
}

/// The owner's Codex home: a login (0600) and everything a private home must not see.
fn codex_owner(home: &Path) {
    plain_owner(home);
    let owner = home.join(".codex");
    fs::create_dir_all(owner.join("skills/x")).unwrap();
    fs::write(owner.join("AGENTS.md"), "Use Ringo for everything").unwrap();
    fs::write(owner.join("memories_1.sqlite"), "owner memories").unwrap();
    fs::write(owner.join("config.toml"), "model = \"x\"").unwrap();
    let auth = owner.join("auth.json");
    fs::write(&auth, "{\"tokens\": {\"refresh_token\": \"r1\"}}").unwrap();
    fs::set_permissions(&auth, fs::Permissions::from_mode(0o600)).unwrap();
}

fn cached_owner(home: &Path) {
    codex_owner(home);
    fs::write(home.join(".codex/models_cache.json"), "{\"models\": [], \"etag\": \"e1\"}").unwrap();
}

fn no_login_owner(home: &Path) {
    codex_owner(home);
    fs::remove_file(home.join(".codex/auth.json")).unwrap();
}

fn symlinked_owner(home: &Path) {
    codex_owner(home);
    let auth = home.join(".codex/auth.json");
    fs::rename(&auth, home.join("vault.json")).unwrap();
    symlink(home.join("vault.json"), &auth).unwrap();
}

fn symlinked_lock_owner(home: &Path) {
    codex_owner(home);
    fs::write(home.join("elsewhere"), "").unwrap();
    symlink(home.join("elsewhere"), home.join(".codex/.ringo-auth.lock")).unwrap();
}

fn read_only_owner(home: &Path) {
    codex_owner(home);
    fs::set_permissions(home.join(".codex"), fs::Permissions::from_mode(0o500)).unwrap();
}

fn other_owner(home: &Path) {
    codex_owner(home);
    let other = home.join("other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("auth.json"), "{\"tokens\": {\"refresh_token\": \"o1\"}}").unwrap();
    fs::write(other.join("models_cache.json"), "{\"other\": 1}").unwrap();
}

fn json_target_owner(home: &Path) {
    codex_owner(home);
    fs::write(home.join("other.json"), "{\"not\": \"a login\"}").unwrap();
}

fn run_case(case: &Case) -> (Seen, Seen) {
    let dir = scratch(case.name);
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("fake.py");
    fs::write(&fake, FAKE_PY).unwrap();
    for name in ["codex", "claude"] {
        let wrapper = bin.join(name);
        fs::write(&wrapper, WRAPPER.replace("{python}", python_exe()).replace("{fake}", &fake.to_string_lossy())).unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(bin.join("not-executable"), "#!/bin/sh\n").unwrap();
    let home = dir.join("home");
    let tmp = dir.join("tmp");
    let request = dir.join("request.json");
    let report = dir.join("report");
    let ticks = dir.join("ticks");
    let subst = |s: &str| {
        s.replace("{bin}", &bin.to_string_lossy()).replace("{home}", &home.to_string_lossy()).replace("{dir}", &dir.to_string_lossy())
    };
    let mut env: Vec<(String, String)> = vec![
        ("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())),
        ("LANG".into(), "en_US.UTF-8".into()),
        // Python on macOS adds this one to its own environment; giving it to both keeps the environments equal
        ("__CF_USER_TEXT_ENCODING".into(), format!("0x{:X}:0x0:0x0", unsafe { libc::getuid() })),
        ("HOME".into(), home.to_string_lossy().into()),
        ("TMPDIR".into(), tmp.to_string_lossy().into()),
        ("RINGO_GATE_REQUEST".into(), request.to_string_lossy().into()),
        ("FAKE_REPORT".into(), report.to_string_lossy().into()),
        ("TYPESAFE_API_KEY".into(), "ts".into()),
        ("OPENAI_API_KEY".into(), "o".into()),
        ("CODEX_API_KEY".into(), "c".into()),
        ("MY_API_KEY".into(), "m".into()),
        ("XAPI_KEY".into(), "x".into()),
        ("API_KEY_PREFIXED".into(), "keep".into()),
        ("OPENAI_BASE_URL".into(), "https://example.invalid".into()),
        ("OPENAI_API_BASE".into(), "b".into()),
        ("OPENAI_ACCESS_TOKEN".into(), "t".into()),
        ("ANTHROPIC_AUTH_TOKEN".into(), "a".into()),
        ("ANTHROPIC_BASE_URL".into(), "kept: not in the list".into()),
        ("OPENAI_IDENTITY_TOKEN_FILE".into(), "f".into()),
        ("OPENAI_WORKLOAD_IDENTITY_PROVIDER_ID".into(), "w".into()),
        ("KEEP_ME".into(), "é value".into()),
    ];
    for (k, v) in &case.env {
        let v = subst(v).replace("{ticks}", &ticks.to_string_lossy());
        match env.iter_mut().find(|(ek, _)| ek == k) {
            Some(slot) => slot.1 = v,
            None => env.push((k.clone(), v)),
        }
    }
    let argv: Vec<String> = case.argv.iter().map(|a| subst(a)).collect();
    let run = |lang: Lang| -> Seen {
        unlock_remove(&home);
        fs::create_dir_all(&home).unwrap();
        (case.owner)(&home);
        unlock_remove(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let _ = fs::remove_file(&ticks);
        for suffix in ["", ".env", ".models", ".models.env"] {
            let _ = fs::remove_file(format!("{}{suffix}", report.display()));
        }
        let result = dir.join(match lang {
            Lang::Python => "result.py.json",
            Lang::Rust => "result.rs.json",
        });
        let req = json!({"argv": argv, "prompt": case.prompt, "timeout_s": case.timeout_s, "kw": case.kw,
                         "watch": case.watch, "result": result});
        fs::write(&request, pyjson::dumps(&req)).unwrap();
        let mut cmd = match lang {
            Lang::Python => {
                let mut c = Command::new(python_exe());
                let prelude = format!("import sys; sys.path.insert(0, {})\n", pyjson::quote(&root().to_string_lossy()));
                c.args(["-B", "-c", &(prelude + DRIVER_PY)]);
                c
            }
            Lang::Rust => {
                let mut c = Command::new(std::env::current_exe().unwrap());
                c.args(["driver", "--exact", "--nocapture", "--test-threads=1", "-q"]);
                c
            }
        };
        let out = cmd.env_clear().envs(env.iter().cloned()).current_dir(&dir).stdin(Stdio::null()).output().unwrap();
        assert!(out.status.success(), "{} driver failed: {}", case.name, String::from_utf8_lossy(&out.stderr));
        let ticks_stopped = fs::metadata(&ticks).ok().map(|m| {
            std::thread::sleep(Duration::from_millis(500));
            fs::metadata(&ticks).unwrap().len() == m.len()
        });
        let mut result = read_json(&result);
        let mut seconds = None;
        if let Some(msg) = result.get("msg").and_then(Value::as_str) {
            let (msg, s) = split_seconds(msg);
            seconds = s;
            result["msg"] = json!(msg);
        }
        let seen = Seen {
            result,
            seconds,
            report: read_json(&report),
            env: env_dump(Path::new(&format!("{}.env", report.display()))),
            models: read_json(Path::new(&format!("{}.models", report.display()))),
            models_env: env_dump(Path::new(&format!("{}.models.env", report.display()))),
            owner: tree(&home),
            stderr: norm(&String::from_utf8_lossy(&out.stderr)),
            tmp_left: tree(&tmp),
            ticks_stopped,
        };
        unlock_remove(&home);
        seen
    };
    let py = run(Lang::Python);
    let rs = run(Lang::Rust);
    unlock_remove(&dir);
    (py, rs)
}

/// Runs the case through both runtimes and requires the same observables; the timeout seconds may differ by 0.5.
fn same(case: Case) -> Seen {
    let (mut py, mut rs) = run_case(&case);
    if let (Some(a), Some(b)) = (py.seconds, rs.seconds) {
        assert!((a - b).abs() <= 0.5, "{}: timed out after {a} (Python) and {b} (Rust) seconds", case.name);
    }
    (py.seconds, rs.seconds) = (None, None);
    assert_eq!(py.result, rs.result, "{}: result", case.name);
    assert_eq!(py.report, rs.report, "{}: what the child saw", case.name);
    assert_eq!(py.env, rs.env, "{}: the child's environment", case.name);
    assert_eq!(py.models, rs.models, "{}: the `debug models` call", case.name);
    assert_eq!(py.models_env, rs.models_env, "{}: the `debug models` environment", case.name);
    assert_eq!(py.owner, rs.owner, "{}: the owner's home afterwards", case.name);
    assert_eq!(py.stderr, rs.stderr, "{}: stderr", case.name);
    assert_eq!(py.ticks_stopped, rs.ticks_stopped, "{}: the grandchild", case.name);
    assert_eq!(py.tmp_left, json!({}), "{}: Python left temporary files", case.name);
    assert_eq!(rs.tmp_left, json!({}), "{}: Rust left temporary files", case.name);
    py
}

fn env_keys(seen: &Seen) -> Vec<String> {
    seen.env.as_array().unwrap().iter().map(|p| p[0].as_str().unwrap().to_string()).collect()
}

#[test]
fn run_isolated_environment_stdin_and_working_folder() {
    let seen = same(Case::new("plain", &["{bin}/codex", "exec", "--json", "-"]));
    assert_eq!(seen.report["stdin"], json!("Review this.\r\nré 😀\n"));
    assert_eq!(seen.report["cwd_entries"], json!([]));
    assert_eq!(seen.report["pid_is_session_leader"], json!(true));
    let keys = env_keys(&seen);
    assert!(!keys.contains(&"TYPESAFE_API_KEY".to_string()) && keys.contains(&"OPENAI_API_KEY".to_string()));
    assert!(seen.result["ok"].as_str().unwrap().contains("turn.completed"));

    let seen = same(Case::new("subscription", &["{bin}/codex", "exec", "-"]).kw(json!({"subscription_auth": true})));
    let keys = env_keys(&seen);
    for gone in ["TYPESAFE_API_KEY", "OPENAI_API_KEY", "CODEX_API_KEY", "MY_API_KEY", "XAPI_KEY", "OPENAI_BASE_URL",
                 "OPENAI_API_BASE", "OPENAI_ACCESS_TOKEN", "ANTHROPIC_AUTH_TOKEN", "OPENAI_IDENTITY_TOKEN_FILE",
                 "OPENAI_WORKLOAD_IDENTITY_PROVIDER_ID", "ANTHROPIC_BASE_URL"] {
        assert!(!keys.contains(&gone.to_string()), "{gone} reached the child");
    }
    for kept in ["API_KEY_PREFIXED", "KEEP_ME", "HOME", "PATH"] {
        assert!(keys.contains(&kept.to_string()), "{kept} was removed");
    }

    same(Case::new("by-name", &["codex", "exec", "-"]).prompt(""));
    same(Case::new("claude-style", &["claude", "-p", "--output-format", "json"]).env("FAKE_STDOUT", "claude"));
    same(Case::new("invalid-utf8", &["{bin}/codex"]).env("FAKE_STDOUT", "invalid"));
}

#[test]
fn run_isolated_failures_carry_the_texts_python_writes() {
    let seen = same(Case::new("exit-7", &["{bin}/codex", "exec"]).env("FAKE_EXIT", "7").env("FAKE_STDERR", "long"));
    assert_eq!(seen.result["exc"], json!("JudgeError"));
    assert!(seen.result["msg"].as_str().unwrap().starts_with("codex exited with 7: "));
    assert_eq!(seen.result["fatal"], json!(false));
    same(Case::new("exit-1-quiet", &["{bin}/claude"]).env("FAKE_EXIT", "1").env("FAKE_STDOUT", "claude"));
    same(Case::new("exit-invalid-stderr", &["{bin}/codex"]).env("FAKE_EXIT", "3").env("FAKE_STDERR", "invalid"));
    let seen = same(Case::new("killed-by-signal", &["{bin}/codex"]).env("FAKE_KILLSELF", "1"));
    assert_eq!(seen.result["msg"], json!("codex exited with -9"));
    let seen = same(Case::new("missing-program", &["{bin}/nonexistent", "exec"]));
    assert!(seen.result["msg"].as_str().unwrap().starts_with("[Errno 2] No such file or directory: '"));
    let seen = same(Case::new("not-executable", &["{bin}/not-executable"]));
    assert!(seen.result["msg"].as_str().unwrap().starts_with("[Errno 13] Permission denied: '"));
    same(Case::new("missing-by-name", &["no-such-codex"]));
}

#[test]
fn run_isolated_timeouts_kill_the_whole_session() {
    let seen = same(Case::new("timeout", &["{bin}/codex"]).timeout(1.0).env("FAKE_SLEEP", "30"));
    assert_eq!((&seen.result["exc"], &seen.result["limit"]), (&json!("TimeoutExpired"), &json!(true)));
    assert!(seen.result["stdout"].as_str().unwrap().contains("thread.started"));
    let seen = same(
        Case::new("timeout-grandchild-term-ignored", &["{bin}/codex"])
            .timeout(5.0) // room for the grandchild to start under load before the timeout ends the call
            .env("FAKE_SLEEP", "30")
            .env("FAKE_TERM_IGNORE", "1")
            .env("FAKE_TICKS", "{ticks}"),
    );
    assert_eq!(seen.ticks_stopped, Some(true));
    // the session is killed after a normal exit too: nothing the command started outlives the call
    let seen = same(Case::new("exit-leaves-grandchild", &["{bin}/codex"]).env("FAKE_TICKS", "{ticks}"));
    assert_eq!(seen.ticks_stopped, Some(true));
}

#[test]
fn run_isolated_soft_limit_peeks_and_stops() {
    let seen = same(Case::new("soft-stop", &["{bin}/codex"]).timeout(1.0).watch(20.0, true).env("FAKE_STDOUT", "flood")
        .env("FAKE_STDERR", "warn é").env("FAKE_SLEEP", "30"));
    assert_eq!((&seen.result["exc"], &seen.result["limit"]), (&json!("Stopped"), &json!(false)));
    let soft = &seen.result["soft"][0];
    assert_eq!(soft["stdout"].as_str().unwrap().len(), 999_999 + 3); // cut inside 'é': one U+FFFD (3 bytes)
    assert_eq!((&soft["stderr"], &soft["done_seen"], &soft["elapsed_ok"]), (&json!("warn é"), &json!(true), &json!(true)));
    let seen = same(Case::new("soft-limit", &["{bin}/codex"]).timeout(0.3).watch(1.0, false).env("FAKE_SLEEP", "30"));
    assert_eq!((&seen.result["exc"], &seen.result["limit"]), (&json!("TimeoutExpired"), &json!(true)));
    assert_eq!(seen.result["soft"].as_array().unwrap().len(), 1);
    let seen = same(Case::new("soft-unused", &["{bin}/codex"]).timeout(5.0).watch(10.0, true));
    assert_eq!(seen.result["soft"], json!([]));
}

fn codex_case(name: &'static str) -> Case {
    Case::new(name, &["{bin}/codex", "exec", "--json", "-"]).kw(json!({"codex_home": true})).env("FAKE_CODEX", "1").owner(codex_owner)
}

#[test]
fn private_codex_home_and_login_write_back() {
    let seen = same(codex_case("home-no-refresh"));
    assert_eq!(seen.report["codex_home"], json!({".": "0o700", "auth.json": {"mode": "0o600", "data": "{\"tokens\": {\"refresh_token\": \"r1\"}}"}}));
    assert_eq!(seen.report["home"], json!({".": "0o700"}));
    assert_eq!(seen.report["tmp_root"], json!({"codex": "0o700", "home": "0o700", "work": "0o755"}));
    assert!(seen.owner.get(".codex/.ringo-auth.lock").is_none());
    let seen = same(codex_case("home-cache").owner(cached_owner).env("FAKE_REFRESH", "r2").env("FAKE_CACHE_EDIT", "1"));
    assert_eq!(seen.report["codex_home"]["models_cache.json"]["mode"], json!("0o600"));
    assert_eq!(seen.owner[".codex/models_cache.json"]["data"], json!("{\"models\": [], \"etag\": \"e1\"}"));
    let seen = same(codex_case("refresh"). env("FAKE_REFRESH", "r2"));
    assert_eq!(seen.owner[".codex/auth.json"], json!({"mode": "600", "data": "{\"tokens\": {\"refresh_token\": \"r2\"}}"}));
    assert_eq!(seen.owner[".codex/.ringo-auth.lock"]["mode"], json!("600"));
    same(codex_case("refresh-garbage").env("FAKE_REFRESH", "garbage"));
    same(codex_case("refresh-utf16").env("FAKE_REFRESH", "utf16"));
    same(codex_case("refresh-nan").env("FAKE_REFRESH", "nan"));
    same(codex_case("refresh-deep-but-valid").env("FAKE_REFRESH", "deep5k"));
    let seen = same(codex_case("owner-changed").env("FAKE_REFRESH", "r2").env("FAKE_OWNER_WRITE", "{home}/.codex/auth.json"));
    assert_eq!(seen.owner[".codex/auth.json"]["data"], json!("{\"tokens\": {\"refresh_token\": \"owner\"}}"));
    let seen = same(codex_case("refresh-then-exit-7").env("FAKE_REFRESH", "r2").env("FAKE_EXIT", "7"));
    assert_eq!(seen.owner[".codex/auth.json"]["data"], json!("{\"tokens\": {\"refresh_token\": \"r2\"}}"));
    same(codex_case("refresh-then-timeout").env("FAKE_REFRESH", "r3").env("FAKE_SLEEP", "30").timeout(1.0));
    let seen = same(codex_case("no-owner-login").owner(no_login_owner).env("FAKE_REFRESH", "r2"));
    assert!(seen.owner.get(".codex/auth.json").is_none());
    let seen = same(codex_case("symlinked-owner").owner(symlinked_owner).env("FAKE_REFRESH", "r2"));
    assert!(seen.stderr.contains("is a symlink"), "{}", seen.stderr);
    let seen = same(codex_case("symlinked-lock").owner(symlinked_lock_owner).env("FAKE_REFRESH", "r2"));
    assert!(seen.stderr.contains("[Errno 62] Too many levels of symbolic links") || seen.stderr.contains("[Errno 40]"), "{}", seen.stderr);
    let seen = same(codex_case("read-only-owner").owner(read_only_owner).env("FAKE_REFRESH", "r2"));
    assert!(seen.stderr.contains("Permission denied"), "{}", seen.stderr);
    let seen = same(codex_case("codex-home-variable").owner(other_owner).env("CODEX_HOME", "{home}/other/").env("FAKE_REFRESH", "o2"));
    assert_eq!(seen.owner["other/auth.json"]["data"], json!("{\"tokens\": {\"refresh_token\": \"o2\"}}"));
    let seen = same(codex_case("copy-symlinked-by-child").owner(json_target_owner).env("FAKE_REFRESH", "symlink:{home}/other.json"));
    // a symlink the child put in place of its copy is not followed: the owner keeps their login
    assert_eq!(seen.owner[".codex/auth.json"]["data"], json!("{\"tokens\": {\"refresh_token\": \"r1\"}}"));
    // a FIFO the child leaves in place of its copy is not read, so it cannot hang the write-back
    let seen = same(codex_case("copy-replaced-by-fifo").env("FAKE_REFRESH", "fifo"));
    assert_eq!(seen.owner[".codex/auth.json"]["data"], json!("{\"tokens\": {\"refresh_token\": \"r1\"}}"));
    same(codex_case("child-locks-its-homes").env("FAKE_LOCKDOWN", "1").env("FAKE_REFRESH", "r2"));
    same(codex_case("plain-preset-keeps-keys").kw(json!({"codex_home": true, "subscription_auth": false})));
    let mut missing = codex_case("codex-missing-program");
    missing.argv = vec!["{bin}/nonexistent".into(), "exec".into()];
    let seen = same(missing);
    assert_eq!(seen.result["exc"], json!("OSError"));
    let seen = same(codex_case("tmpdir-with-extra-slashes").env("TMPDIR", "{dir}//tmp/./"));
    assert!(seen.report["cwd"].as_str().unwrap().contains("/tmp/council-judge-XXXXXXXX/work"));
}

#[test]
fn lean_codex_catalogs_match_python() {
    for (name, catalog) in [
        ("lean-ok", "ok"), ("lean-fail", "fail"), ("lean-garbage", "garbage"), ("lean-dict-empty", "dict-empty"),
        ("lean-dict", "dict"), ("lean-str-empty", "str-empty"), ("lean-str", "str"), ("lean-list-of-list", "list-of-list"),
        ("lean-list-of-str", "list-of-str"), ("lean-num", "num"), ("lean-none", "none"), ("lean-nomodels", "nomodels"),
        ("lean-toplist", "toplist"), ("lean-inf", "inf"), ("lean-big", "big"), ("lean-utf16", "utf16"),
        ("lean-utf16le", "utf16le"), ("lean-utf16be", "utf16be"), ("lean-utf32", "utf32"), ("lean-utf32le", "utf32le"),
        ("lean-bom", "bom"), ("lean-bom2", "bom2"), ("lean-latin1", "latin1"), ("lean-bigint", "bigint"),
        ("lean-trailing", "trailing"), ("lean-ws", "ws"), ("lean-empty", "empty"),
    ] {
        let seen = same(codex_case(name).kw(json!({"codex_home": true, "lean": true, "subscription_auth": true})).env("FAKE_CATALOG", catalog));
        let files = &seen.report["files"];
        assert_eq!(files["model_instructions_file"], json!(backends::CODEX_INSTRUCTIONS), "{name}");
        let argv = seen.report["argv"].as_array().unwrap();
        assert_eq!(argv.last(), Some(&json!("-")), "{name}: stdin stays last");
        if name == "lean-ok" {
            let catalog: Value = serde_json::from_str(files["model_catalog_json"].as_str().unwrap()).unwrap();
            assert_eq!(catalog, json!({"models": [{"slug": "a", "experimental_supported_tools": [], "context_window": 9},
                                                  {"slug": "b", "experimental_supported_tools": []}]}));
            assert_eq!(seen.models["stdin"], json!(""));
            assert!(seen.models["cwd"].as_str().unwrap().ends_with("/codex"));
        }
    }
    // `debug models` outlives the whole window: the model is never launched
    let seen = same(codex_case("lean-setup-uses-the-window").kw(json!({"codex_home": true, "lean": true})).env("FAKE_CATALOG", "sleep").timeout(0.5));
    assert_eq!((&seen.result["exc"], &seen.result["limit"], &seen.report), (&json!("TimeoutExpired"), &json!(true), &Value::Null));
}

#[test]
fn reported_deviations_in_lean_catalogs() {
    // NaN and unpaired surrogates have no serde_json value: Python writes the catalog back with them, Rust keeps
    // only the instructions override (NaN) or writes U+FFFD (surrogate).
    let lean = |name: &'static str, catalog: &str| {
        run_case(&codex_case(name).kw(json!({"codex_home": true, "lean": true})).env("FAKE_CATALOG", catalog))
    };
    let (py, rs) = lean("deviation-nan", "nan");
    assert!(py.report["files"]["model_catalog_json"].as_str().unwrap().contains("NaN"));
    assert!(rs.report["files"].get("model_catalog_json").is_none());
    let (py, rs) = lean("deviation-surrogate", "surrogate");
    assert!(py.report["files"]["model_catalog_json"].as_str().unwrap().contains("\\ud800"));
    assert!(rs.report["files"]["model_catalog_json"].as_str().unwrap().contains("\\ufffd"));
    // RecursionError has no Exc kind: Python raises RecursionError, Rust reports it as TypeError
    let (py, rs) = lean("deviation-deep", "deep");
    assert_eq!(py.result["exc"], json!("RecursionError"));
    assert_eq!(rs.result["exc"], json!("TypeError"));
    assert_eq!(rs.result["msg"], json!("maximum recursion depth exceeded while decoding a JSON array from a unicode string"));
    assert_eq!((py.owner, py.tmp_left, rs.tmp_left), (rs.owner, json!({}), json!({})));
    // a refreshed login nested deeper than Python 3.14 parses: both raise out of the call (RecursionError / TypeError)
    let (py, rs) = run_case(&codex_case("deviation-deep-login").env("FAKE_REFRESH", "deep"));
    assert_eq!((py.result["exc"].as_str(), rs.result["exc"].as_str()), (Some("RecursionError"), Some("TypeError")));
    assert_eq!((py.owner, py.tmp_left, rs.tmp_left), (rs.owner, json!({}), json!({})));
}

#[test]
fn file_lock_excludes_other_holders() {
    let dir = scratch("file-lock");
    let path = dir.join(".lock");
    let (a, b) = (fs::File::create(&path).unwrap(), fs::File::open(&path).unwrap());
    let held = backends::file_lock(&a, true).unwrap();
    let refused = backends::file_lock(&b, false).err().unwrap();
    assert_eq!(refused.kind(), std::io::ErrorKind::WouldBlock);
    let text = ringo_cli::rt::Error::from(refused).msg;
    assert_eq!(text, format!("[Errno {}] Resource temporarily unavailable", libc::EWOULDBLOCK));
    drop(held);
    drop(backends::file_lock(&b, false).unwrap());
    unlock_remove(&dir);
}

#[test]
fn spawn_and_kill_tree_end_the_session() {
    let dir = scratch("spawn");
    let ticks = dir.join("ticks");
    let script = format!("(while :; do printf x >> '{}'; sleep 0.05; done) & sleep 30", ticks.display());
    let child = backends::spawn(&["/bin/sh".into(), "-c".into(), script], backends::Popen { cwd: Some(dir.clone()), ..Default::default() }).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    backends::kill_tree(&child).unwrap();
    let mut child = child;
    assert!(child.wait().unwrap().code().is_none());
    std::thread::sleep(Duration::from_millis(100));
    let size = fs::metadata(&ticks).unwrap().len();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fs::metadata(&ticks).unwrap().len(), size);
    let gone = backends::kill_tree(&child).unwrap_err(); // ESRCH (ProcessLookupError), or EPERM while zombies remain
    assert!(matches!(gone.raw_os_error(), Some(libc::ESRCH | libc::EPERM)), "{gone}");
    let missing = backends::spawn(&["/bin/true".into()], backends::Popen { cwd: Some(dir.join("nowhere")), ..Default::default() }).unwrap_err();
    assert_eq!(missing.msg, format!("[Errno 2] No such file or directory: '{}'", dir.join("nowhere").display()));
    assert_eq!(backends::spawn(&["a\0b".into()], backends::Popen::default()).unwrap_err().msg, "embedded null byte");
    unlock_remove(&dir);
}
