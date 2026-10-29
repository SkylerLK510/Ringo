//! Differential gate for `rt::findings`: each test runs findings.py on the same inputs through python3 and compares
//! the output byte for byte with the Rust port. Offline (temporary git repositories); nothing is normalised.
#![cfg(unix)]

use ringo_cli::rt::findings as f;
use ringo_cli::rt::{Error, pyjson};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DRIVER: &str = r####"
import json, sys
sys.path.insert(0, sys.argv[1])
import findings
def lines(anchors):
    return [list(a) for a in anchors]
for fn, a in json.loads(sys.stdin.read()):
    try:
        if fn == 'parse': r = findings.parse(a[0])
        elif fn == 'locations': r = lines(findings.locations(a[0], a[1]))
        elif fn == 'heading': r = lines(findings.heading_anchors(a[0]))
        elif fn == 'field': r = findings.field(a[0], a[1])
        elif fn == 'problem': r = findings.problem(a[0])
        elif fn == 'clean': r = findings.clean(a[0])
        elif fn == 'anchor': r = list(findings.anchor(a[0], a[1], a[2], a[3]))
        elif fn == 'reviewbench': r = findings.reviewbench(a[0], a[1], a[2])
        out = {'ok': r}
    except Exception as e:
        out = {'err': [type(e).__name__, str(e)]}
    sys.stdout.write(json.dumps(out) + '\n')
"####;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn python(cases: &[Value]) -> Vec<String> {
    let mut child = Command::new("python3")
        .args(["-c", DRIVER])
        .arg(root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 runs");
    let input = pyjson::dumps(&Value::Array(cases.to_vec()));
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().lines().map(String::from).collect()
}

fn type_name(e: &Error) -> String {
    ringo_cli::rt::harness::type_name(e)
}

fn result(r: Result<Value, Error>) -> String {
    pyjson::dumps(&match r {
        Ok(v) => json!({"ok": v}),
        Err(e) => json!({"err": [type_name(&e), e.msg]}),
    })
}

fn anchors(a: Vec<f::Anchor>) -> Value {
    Value::Array(a.into_iter().map(|(p, s, e)| json!([p, s.to_value(), e.to_value()])).collect())
}

fn entries(v: &Value) -> f::Entries {
    v.as_array().unwrap().iter().map(|x| x.as_object().unwrap().clone()).collect()
}

fn rust(case: &Value) -> String {
    let (name, a) = (case[0].as_str().unwrap(), &case[1]);
    let s = |i: usize| a[i].as_str().unwrap();
    result(match name {
        "parse" => f::parse(s(0)).map(|e| Value::Array(e.into_iter().map(Value::Object).collect())),
        "locations" => f::locations(s(0), a[1].as_bool().unwrap()).map(anchors),
        "heading" => f::heading_anchors(s(0)).map(anchors),
        "field" => Ok(f::field(s(0), s(1)).map_or(Value::Null, |x| json!(x))),
        "problem" => {
            let lines: Vec<&str> = a[0].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
            Ok(f::problem(&lines).map_or(Value::Null, Value::String))
        }
        "clean" => Ok(json!(f::clean(s(0)))),
        "anchor" => f::anchor(&entries(&a[0]), Path::new(s(1)), s(2), a[3].as_str()).map(|(k, d)| {
            json!([Value::Array(k.into_iter().map(Value::Object).collect()), Value::Array(d.into_iter().map(Value::Object).collect())])
        }),
        "reviewbench" => f::reviewbench(&entries(&a[0]), a[1].as_object().unwrap(), s(2)),
        other => panic!("{other}"),
    })
}

fn compare(what: &str, cases: &[Value]) -> Vec<String> {
    let py = python(cases);
    assert_eq!(py.len(), cases.len());
    let mut bad = vec![];
    for (i, case) in cases.iter().enumerate() {
        let rs = rust(case);
        if rs != py[i] {
            bad.push(format!("case {i}: {}\n  python: {}\n  rust:   {rs}", pyjson::dumps(case), py[i]));
        }
    }
    assert!(bad.is_empty(), "{what}: {} of {} differ\n{}", bad.len(), cases.len(), bad.iter().take(4).cloned().collect::<Vec<_>>().join("\n"));
    eprintln!("{what}: {} cases identical", cases.len());
    py
}

const REVIEW: &str = "What this change does: x.

### 1. Cache is read without the lock
- Location: `src/pool.py:4-6`
- Severity: High
- Category: concurrency
- Code: `conns = self.conns`
- Problem: two threads call get() at once;
  both read the same connection.
  - Nested detail stays.
- Fix: hold the lock.
```
### 6. Not a finding inside a fence (`fenced.py:1`)
```

### 2. Workflow and docs disagree (`.github/workflows/ci.yml:~12`, `docs/guide.md:3-9`; `:20-22`)
- Code: `x`
- Problem: the docs say y.

### 3. `lsmysql.New` panics on nil (`README.md`)
- Problem: z.

### 4. Off by one in `d.db` handling
Body text mentions `d.db` first, then `./src/pool.py:2` where it happens.

### 5. Nothing to anchor
- Problem: no file at all.

### Open questions
### 7. Numbered question (`q.py:1`)

### Not checked
- `n.py:3`
";

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
    fn chance(&mut self, p: u64) -> bool {
        self.next() % 100 < p
    }
}

const PATHS: &[&str] = &[
    "src/pool.py:4-6", ".\\src\\a.py:9-3", "/b/c.ts#L4-L5", "Makefile:2", "x.py:L7", "x.py: line 7", "x.py: lines 3 - 9",
    "x.py:~12", "y.R:2", "file.tar.gz:1", "naïve/ünï.py:3", "x.py:٣", "x.py:007", "x.py:99999999999999999999999999999999999999999",
    "a.b", "dir/", "foo@1.2/bar.py:3", "[x]/y.py", "a+b.py", "a-b_c.json#L2-L3", "C:\\x\\y.py", "README.md", "Dockerfile:3",
    "pkg/core.py:12–14", "d.db", "a/README.md:5", "b/src/pool.py:1", "SKILL.md:2", "pool.py:0-2", "x.PY:3", "e.Rmd:1",
    "lib/x.rs:0", "a.py:12-L15", "v1.2", "img.png", "src/", "x.py:12 -15", "(nested.py:3)",
];
const TITLES: &[&str] = &[
    "Cache is read without the lock", "Off by one in `d.db` handling", "`lsmysql.New` panics on nil", "Ünïcödé title ✓",
    "Trailing dots...", "**Bold title**", "Title with (parenthesis) inside", "Mentions `pkg/x.go:12` inline", "",
];
const FIELDS: &[&str] = &[
    "- Location: `{P}`", "- **Location:** {P}", "* location : `{P}`", "- LOCAT\u{130}ON: `{P}`", "- Location:", "- Severity: High",
    "- severity: **CRITICAL**", "- Category: Concurrency", "- **Category**: Data loss", "- Code: `x = 1`", "- Fix: hold the lock.",
    "- Problem: two threads read it;\n  both see stale data.\n  - Nested detail stays.\n- Fix: lock.",
    "- **Problem**: short.", "* Problem : wrapped\n\nNew paragraph after a blank line.", "- PROBLEM: upper\n### not a field",
    "- Problem:\n  starts on the next line", "Body text mentions `{P}` where it happens.", "See {P} and `{P}` too.",
    "  - Problem: indented bullet\n  - Other: stops", "- Problem: tabs\tand\u{a0}spaces\u{1c}here",
];

fn review(rng: &mut Rng) -> String {
    let mut out = String::new();
    if rng.chance(50) {
        out.push_str("What this change does: things.\n\n");
    }
    let n = rng.next() % 5;
    for k in 1..=n {
        let heading = rng.pick(&["### {n}. {T}", "## {n}) {T}", "#### F{n}: {T}", "### **{n}. {T}**", "###{n}.{T}"]);
        let mut title = rng.pick(TITLES).to_string();
        if rng.chance(60) {
            let anchors: Vec<String> = (0..1 + rng.next() % 3).map(|_| format!("`{}`", rng.pick(PATHS))).collect();
            title.push_str(&format!(" ({})", anchors.join(if rng.chance(50) { ", " } else { "; " })));
        }
        out.push_str(&heading.replace("{n}", &k.to_string()).replace("{T}", &title));
        out.push('\n');
        for _ in 0..rng.next() % 5 {
            out.push_str(&rng.pick(FIELDS).replace("{P}", rng.pick(PATHS)));
            out.push('\n');
        }
        if rng.chance(15) {
            out.push_str("```\n### 9. fenced (`f.py:1`)\n```\n");
        }
        out.push('\n');
    }
    if rng.chance(30) {
        out.push_str("### Open questions\n- Does a caller rely on `old.py:3`?\n### 8. Numbered question (`q.py:1`)\n");
    }
    if rng.chance(30) {
        out.push_str("### Not checked\n- `n.py:3`\n");
    }
    if rng.chance(15) {
        out = out.replace('\n', "\r\n");
    }
    out
}

#[test]
fn parse_and_its_helpers_match_python() {
    let mut rng = Rng(0xf1d1_0000_0000_0007);
    let mut cases = vec![json!(["parse", [REVIEW]]), json!(["parse", [""]]), json!(["parse", ["No defects found.\n\n### Not checked\n- `a.py:1`"]])];
    cases.push(json!(["parse", ["### 1. T (`.\\src\\a.py:9-3`, `/b/c.ts#L4-L5`, `Makefile:2`, `x.py:L7`)\n- Problem: p"]]));
    cases.push(json!(["parse", [format!("### 1. T (`a.py:3`)\nJust prose {}", "x".repeat(3000))]]));
    cases.push(json!(["parse", [format!("### 1. T (`a.py:{}`)\n", "9".repeat(4301))]]));
    cases.push(json!(["parse", [format!("### 1. T (`a.py:{}`)\n", "٣".repeat(40))]]));
    for _ in 0..1500 {
        cases.push(json!(["parse", [review(&mut rng)]]));
    }
    for p in PATHS {
        for text in [p.to_string(), format!("`{p}`"), format!("see {p}, then `{p}`."), format!("({p})")] {
            cases.push(json!(["locations", [text, false]]));
            cases.push(json!(["locations", [text, true]]));
            cases.push(json!(["heading", [format!("Title ({text})")]]));
            cases.push(json!(["heading", [format!("Title `{text}` here")]]));
        }
        cases.push(json!(["clean", [p]]));
    }
    for field in FIELDS {
        let body = field.replace("{P}", "a.py:3");
        for name in ["Location", "severity", "category"] {
            cases.push(json!(["field", [body, name]]));
        }
        let lines: Vec<&str> = ringo_cli::rt::py::splitlines(&body, false);
        cases.push(json!(["problem", [lines]]));
    }
    let py = compare("findings.parse and helpers", &cases);
    let entries: usize = py.iter().filter(|l| l.starts_with(r#"{"ok": [{"#)).count();
    assert!(entries > 500, "{entries}");
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn anchor_and_reviewbench_match_python() {
    let dir = std::env::temp_dir().join(format!("ringo-gate-findings-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let repo = dir.join("repo");
    for (path, text) in [
        ("src/pool.py", "a\nb\nc\nd\ne\nf\ng\n"), ("README.md", "# R\n"), (".github/workflows/ci.yml", "on: push\n"),
        ("bin.dat", "\0\0"), ("docs/SKILL.md", "d\n"), ("skills/SKILL.md", "s\n"), ("empty.txt", ""), ("no-newline.txt", "a\nb"),
        ("naïve/ünï.py", "x\n"), ("Makefile", "all:\n\techo\n"),
    ] {
        let p = repo.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
    }
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["-c", "commit.gpgsign=false", "commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("src/pool.py"), "a\nb\nc\nd\ne\nf\ng\nh\n").unwrap();
    fs::write(repo.join("skills/SKILL.md"), "s\nt\n").unwrap();
    git(&repo, &["-c", "commit.gpgsign=false", "commit", "-qam", "head"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let item = |file: &str, a: &str, b: &str| v(&format!(r#"{{"file": {}, "start_line": {a}, "end_line": {b}, "message": "m", "finding": 1}}"#, pyjson::quote(file)));
    let mut items = vec![];
    for (file, a, b) in [
        ("src/pool.py", "7", "99"), ("docs/guide.md", "1", "1"), ("a/README.md", "5", "3"), ("pool.py", "0", "2"), ("ci.yml", "1", "1"),
        ("bin.dat", "4", "9"), ("SKILL.md", "2", "2"), ("empty.txt", "3", "4"), ("no-newline.txt", "2", "9"), ("ünï.py", "1", "1"),
        ("./src\\pool.py", "123456789012345678901234567890", "1"), ("Makefile", "2", "1"), ("b/Makefile", "1", "1"), ("", "1", "1"),
    ] {
        items.push(item(file, a, b));
    }
    let repo_s = repo.to_string_lossy().to_string();
    let mut cases = vec![];
    for (h, b) in [(head.clone(), Value::Null), (head[..10].to_string(), Value::Null), (head.clone(), json!(base)), (base.clone(), Value::Null),
                   ("--output=x".to_string(), Value::Null), ("nonexistent".to_string(), Value::Null), (head.clone(), json!("nope"))] {
        cases.push(json!(["anchor", [items, repo_s, h, b]]));
        for it in &items {
            cases.push(json!(["anchor", [[it], repo_s, h, b]]));
        }
    }
    let parsed: Value = serde_json::from_str(&python(&[json!(["parse", [REVIEW]])])[0]).unwrap();
    let found = parsed["ok"].clone();
    cases.push(json!(["anchor", [found, repo_s, head, base]]));
    for pr in [
        r#"{"repo": "https://github.com/o/r", "pr_number": "12", "base": "b", "head": "h", "title": "t"}"#, "{}", r#"{"pr_number": 12}"#,
        r#"{"pr_number": "٣"}"#, r#"{"pr_number": "²"}"#, r#"{"pr_number": ""}"#, r#"{"pr_number": null}"#, r#"{"pr_number": "0012"}"#,
        r#"{"pr_number": "abc"}"#, r#"{"pr_number": 1.5, "repo": null}"#,
    ] {
        cases.push(json!(["reviewbench", [found, v(pr), "me"]]));
        cases.push(json!(["reviewbench", [[], v(pr), "x"]]));
    }
    cases.push(json!(["reviewbench", [found, {"pr_number": "9".repeat(4301)}, "me"]]));
    compare("findings.anchor and reviewbench", &cases);
    fs::remove_dir_all(&dir).unwrap();
}

fn v(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}
