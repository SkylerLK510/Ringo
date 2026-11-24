//! Differential gate: `rt::context` against `context.py` through python3, offline, byte for byte. Inputs: synthetic
//! repositories built here from git objects (no work tree, no network), this repository's own history, and the
//! Python standard library (the ignored sweep).
#![cfg(unix)]

use ringo_cli::rt::{context, Error};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// The oracle: calls context.py's functions; `build` runs at the stack depth `python3 ringo.py review` gives it
/// (<module>, ringo.main, harness.main, task, repo_material, build), which decides where RecursionError starts.
const ORACLE: &str = r#"
import inspect, json, sys
sys.path.insert(0, sys.argv[1])
import context

def conv(x):
    if isinstance(x, (set, frozenset)):
        return sorted(conv(i) for i in x)
    if isinstance(x, (tuple, list)):
        return [conv(i) for i in x]
    if isinstance(x, dict):
        return {k: conv(v) for k, v in x.items()}
    if isinstance(x, bytes):
        return x.hex()
    return x

def d5(*a):
    return context.build(*a)

def d4(*a):
    return d5(*a)

def build(*a):  # frame 3 (<module>, main, build), so context.build runs at frame 6 as under ringo.py
    return d4(*a)

def objects_read(repo, reads):
    with context.Objects(repo) as o:
        return [o.read(rev, path) for rev, path in reads]

def cp(points, test):
    return [c for c in points if test(chr(c))]

def first_hunk_olds(c):
    return sorted(context.hunks(None, None, None, None, None, text=f'@@ -{chr(c)} +1 @@')[0])

FUNCS = {
    'build': build,
    'text_symbols': lambda h, c: context.text_symbols(h, c),
    'text_symbols_each': lambda lines: [context.text_symbols([], [l]) for l in lines],
    'kind_each': lambda pairs: [context.kind(l, s) for l, s in pairs],
    'is_test_each': lambda paths: [context.is_test(p) for p in paths],
    'windows': lambda t, l, r: context.windows(t, set(l), r),
    'hunks_text': lambda t: context.hunks(None, None, None, None, None, text=t),
    'new_points': context.new_points,
    'cut_line_each': lambda rows: [context.cut_line(r) for r in rows],
    'cut_long_lines': context.cut_long_lines,
    'generated_kind_each': lambda cases: [context.generated_kind(p, t, b, d) for p, t, b, d in cases],
    'replace_generated': lambda diff, pairs, stats: (lambda r: [r[0], sorted(r[1])])(context.replace_generated(diff, [tuple(p) for p in pairs], {i: tuple(s) for i, s in stats})),
    'line_rows': context.line_rows,
    'changed_windows': lambda path, rows, points, width: context.changed_windows(path, rows, set(points), width),
    'diff_texts': lambda repo, mb, head, pairs: context.diff_texts(repo, mb, head, [tuple(p) for p in pairs]),
    'describe': lambda why: context.describe({tuple(w) for w in why}),
    'listing': context.listing,
    'symbols': lambda repo, mb, head, pairs: context.symbols(repo, mb, head, [tuple(p) for p in pairs]),
    'matches': lambda repo, head, s, o, e: context.matches(repo, head, s, o, set(e)),
    'rank': lambda found: [p for p, _ in context.rank({p: {'lines': set(e['lines']), 'why': {tuple(w) for w in e['why']}}
                                                      for p, e in found.items()})],
    'grep': context.grep,
    'changes': context.changes,
    'all_hunks': lambda repo, mb, head, pairs: context.all_hunks(repo, mb, head, [tuple(p) for p in pairs]),
    'tree_entry': context.tree_entry,
    'blob': context.blob,
    'objects_read': objects_read,
    'commit': context.commit,
    'complete': lambda repo, revs: context.complete(repo, *revs),
    'python_symbols': lambda src, lines: context.python_symbols(src, set(lines)),
    'getsource': lambda: inspect.getsource(context.python_symbols),
    'cp_space': lambda p: cp(p, lambda c: bool(context.text_symbols([], [c + 'ABC = 1']))),
    'cp_word': lambda p: cp(p, lambda c: context.is_test('a_test.' + c)),
    'cp_boundary': lambda p: cp(p, lambda c: context.kind('def abc' + c, 'abc') == 'definition'),
    'cp_digit': lambda p: [[c, first_hunk_olds(c)] for c in p if first_hunk_olds(c)],
    'cp_repr': lambda chunk: [str(e) for e in [_err(lambda: context.commit('.', '-' + ''.join(map(chr, c)), 'base'))
                                               for c in chunk]],
}

def _err(f):
    try:
        f()
    except Exception as exc:
        return exc

def main():
    out = []
    for name, args in json.loads(sys.stdin.buffer.read()):
        try:
            out.append({'ok': conv(FUNCS[name](*args))})
        except Exception as exc:
            out.append({'err': [type(exc).__name__, str(exc)]})
    sys.stdout.write(json.dumps(out))

main()
"#;

fn oracle(calls: &[(&str, Value)]) -> Vec<Value> {
    let input = Value::Array(calls.iter().map(|(n, a)| json!([n, a])).collect());
    let mut child = Command::new(ringo_cli::python())
        .args(["-P", "-c", ORACLE])
        .arg(root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "the oracle failed");
    serde_json::from_slice::<Vec<Value>>(&out.stdout).unwrap()
}

fn one(name: &str, args: Value) -> Value {
    oracle(&[(name, args)]).pop().unwrap()
}

fn err(e: &Error) -> Value {
    json!({"err": [e.type_name(), e.msg]})
}

macro_rules! res {
    ($e:expr) => {
        match $e {
            Ok(v) => json!({"ok": v}),
            Err(e) => err(&e),
        }
    };
}

// ---------------------------------------------------------------------------------------------------------------
// Repositories made from git objects: each commit lists its whole tree (path bytes, mode, content or gitlink id).

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn tmp_root() -> PathBuf {
    std::env::temp_dir().join(format!("ringo-gate-context-{}", std::process::id()))
}

extern "C" fn remove_tmp_root() {
    let _ = std::fs::remove_dir_all(tmp_root());
}

fn tmp(name: &str) -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: registers a plain function that runs once at process exit.
    ONCE.call_once(|| unsafe {
        libc::atexit(remove_tmp_root);
    });
    let d = tmp_root().join(format!("{name}-{}", SEQ.fetch_add(1, Ordering::SeqCst)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn g(dir: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn init(name: &str) -> PathBuf {
    let d = tmp(name);
    g(&d, &["init", "-q"], b"");
    d
}

type File = (Vec<u8>, &'static str, Vec<u8>);

fn f(path: &str, text: &str) -> File {
    (path.as_bytes().to_vec(), "100644", text.as_bytes().to_vec())
}

fn fb(path: &[u8], mode: &'static str, data: &[u8]) -> File {
    (path.to_vec(), mode, data.to_vec())
}

fn commit(repo: &Path, files: &[File], parents: &[&str]) -> String {
    g(repo, &["read-tree", "--empty"], b"");
    let mut info = Vec::new();
    for (path, mode, data) in files {
        let id = if *mode == "160000" {
            String::from_utf8(data.clone()).unwrap()
        } else {
            g(repo, &["hash-object", "-w", "--stdin"], data)
        };
        info.extend(format!("{mode} {id}\t").as_bytes());
        info.extend(path);
        info.push(0);
    }
    g(repo, &["update-index", "-z", "--index-info"], &info);
    let tree = g(repo, &["write-tree"], b"");
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "c"];
    for p in parents {
        args.extend(["-p", p]);
    }
    g(repo, &args, b"")
}

/// A build mode and changed window; None is context.py's default call (auto, CHANGED_WINDOW).
type Mode<'a> = Option<(&'a str, i64)>;

fn rust_build(repo: &Path, base: &str, head: &str, pr: Option<&str>, budget: i64, mode: Mode) -> Value {
    match mode {
        None => res!(context::build(repo, base, head, pr, budget)),
        Some((m, w)) => res!(context::build_mode(repo, base, head, pr, budget, m, w)),
    }
}

/// Builds every case in Rust and in Python and compares; returns the number of cases. The cases that predate the
/// modes are checked in 'full' mode, which must stay what it was.
fn same_builds(cases: &[(PathBuf, String, String, Option<String>, i64)]) -> usize {
    same_builds_mode(cases, Some(("full", context::CHANGED_WINDOW)))
}

fn same_builds_mode(cases: &[(PathBuf, String, String, Option<String>, i64)], mode: Mode) -> usize {
    let calls: Vec<(&str, Value)> = cases
        .iter()
        .map(|(r, b, h, p, n)| {
            let mut args = vec![json!(r.to_str().unwrap()), json!(b), json!(h), json!(p), json!(n)];
            if let Some((m, w)) = mode {
                args.extend([json!(m), json!(w)]);
            }
            ("build", Value::Array(args))
        })
        .collect();
    let want = oracle(&calls);
    for ((r, b, h, p, n), want) in cases.iter().zip(&want) {
        let got = rust_build(r, b, h, p.as_deref(), *n, mode);
        if &got != want {
            let (g, w) = (got.to_string(), want.to_string());
            let at = g.bytes().zip(w.bytes()).take_while(|(a, b)| a == b).count();
            panic!(
                "build({r:?}, {b}, {h}, pr={p:?}, budget={n}, mode={mode:?}) differs at byte {at}:\n rust:   {}\n python: {}",
                &g[at.saturating_sub(200)..(at + 200).min(g.len())],
                &w[at.saturating_sub(200)..(at + 200).min(w.len())]
            );
        }
    }
    cases.len()
}

fn material_len(repo: &Path, base: &str, head: &str, budget: i64) -> i64 {
    context::build_mode(repo, base, head, None, budget, "full", context::CHANGED_WINDOW).unwrap().len() as i64
}

/// Bytes of the change alone (PR text, diff, changed files, README) in full mode.
fn change_alone(repo: &Path, base: &str, head: &str) -> i64 {
    let m = context::build_mode(repo, base, head, None, 1, "full", context::CHANGED_WINDOW).unwrap();
    m.split("\n=== context manifest ===").next().unwrap().len() as i64
}

const PRS: [Option<&str>; 5] =
    [None, Some(""), Some("Bump scale"), Some("Ünïcödé — ☃ 𝔘\r\nline two\r\n"), Some("x\u{2028}y\u{0c}z\n")];

// ---------------------------------------------------------------------------------------------------------------
// The fixture of tests/test_context.py, and budgets across the whole truncation range.

fn fixture(extra: &[File]) -> (PathBuf, String, String) {
    let repo = init("fixture");
    let base_files = [
        f("a.py", "LIMIT = 3\n\n\nclass Shape:\n    def area(self, x):\n        return x * LIMIT\n\n\ndef scale(x):\n    return x + 1\n\n\ndef retired(x):\n    return x\n"),
        f("callers.py", "from a import scale\n\n\ndef grow():\n    return scale(2)\n"),
        f("square.py", "from a import Shape\n\n\nclass Square(Shape):\n    pass\n"),
        f("tests/test_a.py", "from a import scale\n\n\ndef test_scale():\n    assert scale(1) == 2\n"),
        f("old_user.py", "from a import retired\n\nVALUE = retired(1)\n"),
        f("unrelated.py", "def nothing():\n    return None\n"),
        f("web/cart.js", "function computeTotal(items) {\n  return items.length;\n}\n"),
        f("web/page.js", "const n = computeTotal([1, 2]);\n"),
        f("README.md", "# Demo\n"),
    ];
    let mut files: Vec<File> = base_files.to_vec();
    files.extend(extra.iter().cloned());
    let base = commit(&repo, &files, &[]);
    let head_a = "LIMIT = 3\n\n\nclass Shape:\n    def area(self, x):\n        return x * LIMIT * 2\n\n\ndef scale(x):\n    return x + 2\n";
    files[0] = f("a.py", head_a);
    files[6] = f("web/cart.js", "function computeTotal(items) {\n  return items.length + 1;\n}\n");
    files.push(f("NOTES.md", "The function unrelated_prose is not a symbol.\n"));
    let head = commit(&repo, &files, &[&base]);
    (repo, base, head)
}

/// Budgets from just under the change alone to just over the whole material, `steps` of them.
fn budget_sweep(repo: &Path, base: &str, head: &str, steps: i64) -> Vec<(PathBuf, String, String, Option<String>, i64)> {
    let full = material_len(repo, base, head, context::BUDGET);
    let required = material_len(repo, base, head, 1);
    let step = ((full - required + 20) / steps).max(1);
    (0..=steps)
        .map(|k| (repo.to_path_buf(), base.to_string(), head.to_string(), None, required - 10 + k * step))
        .collect()
}

#[test]
fn fixture_material_is_identical_at_every_budget() {
    let (repo, base, head) = fixture(&[]);
    let full = material_len(&repo, &base, &head, context::BUDGET);
    let required = material_len(&repo, &base, &head, 1);
    let mut cases = budget_sweep(&repo, &base, &head, 25);
    for pr in PRS {
        for budget in [context::BUDGET, full - 120, required, 10, 0] {
            cases.push((repo.clone(), base.clone(), head.clone(), pr.map(str::to_string), budget));
        }
    }
    for budget in [200_000, full, full - 1, full + 1, 1, -1] {
        cases.push((repo.clone(), base.clone(), head.clone(), None, budget));
    }
    // short revisions, a reversed range, an empty range
    cases.push((repo.clone(), base[..12].to_string(), head.clone(), None, context::BUDGET));
    cases.push((repo.clone(), head.clone(), base.clone(), None, context::BUDGET));
    cases.push((repo.clone(), head.clone(), head.clone(), Some("same".into()), context::BUDGET));
    assert!(same_builds(&cases) > 55);
}

#[test]
fn many_left_out_files_are_summarized_identically() {
    // test_context.py's long left-out list, plus more than LISTED context files and symbols
    let extra: Vec<File> = (0..150)
        .map(|i| f(&format!("pkg/{}{i}/user_{i}.py", "long_directory_name_".repeat(3)), "from a import scale\nV = scale(1)\n"))
        .collect();
    let (repo, base, head) = fixture(&extra);
    let required = material_len(&repo, &base, &head, 10);
    let mut cases = Vec::new();
    for budget in [required + 2500, context::BUDGET] {
        cases.push((repo.clone(), base.clone(), head.clone(), None, budget));
    }
    // 205 changed functions, each used by its own file: lists past LISTED in every manifest section
    let r2 = init("many");
    let funcs = |k: usize| (0..205).map(|i| format!("def func_{i:03}(x):\n    return x + {k}\n\n\n")).collect::<String>();
    let mut files: Vec<File> = vec![f("lib.py", &funcs(1))];
    files.extend((0..205).map(|i| f(&format!("use/u{i:03}.py"), &format!("from lib import func_{i:03}\nY = func_{i:03}(1)\n"))));
    let b2 = commit(&r2, &files, &[]);
    files[0] = f("lib.py", &funcs(2));
    let h2 = commit(&r2, &files, &[&b2]);
    let req2 = material_len(&r2, &b2, &h2, 10);
    for budget in [context::BUDGET, req2 + 3000, 10] {
        cases.push((r2.clone(), b2.clone(), h2.clone(), None, budget));
    }
    let m = context::build_mode(&r2, &b2, &h2, None, context::BUDGET, "full", context::CHANGED_WINDOW).unwrap();
    assert!(m.contains("- ... and 5 more\n") && m.contains(", ... and 5 more."), "LISTED summaries");
    same_builds(&cases);
}

// ---------------------------------------------------------------------------------------------------------------
// Edge cases: renames, deletions, binaries, gitlinks, non-UTF-8, odd paths, windows, ties, prose, READMEs.

fn edge_repo() -> (PathBuf, Vec<String>) {
    let repo = init("edge");
    // gitlinks to commits in this repository (one whose commit is missing is an error case below)
    let gitlink_a = commit(&repo, &[f("g.txt", "a\n")], &[]);
    let gitlink_b = commit(&repo, &[f("g.txt", "b\n")], &[]);
    let big = |marker: &str| {
        (1..=3000)
            .map(|i| match i {
                10 | 500 | 520 | 1500 | 2990 => format!("TOTAL_{i} = scale(5)  # {marker}\n"),
                _ => format!("filler_{i} = {i}\n"),
            })
            .collect::<String>()
    };
    let a0 = "\
import os\n\nLIMIT: int = 3\nMAX_ITEMS = OTHER = 10\n\n\n@decorator(\n    1)\nclass Shape(Base):\n    SIZE = 2\n\n    def __init__(self):\n        self.n = 1\n\n    def area(self, x):\n        def helper():\n            return 1\n        return x * LIMIT\n\n\nasync def scale(x):\n    return x + 1\n\n\ndef retired(x):\n    return x\n\n\ndef ﬁle_name():\n    return 'nfkc'\n\n\ndef café():\n    return 'é'\n\n\ndef alpha_one():\n    return 1\n\n\ndef beta_two():\n    return 2\n\n\ndef gamma_six():\n    return 6\n\n\ndef delta_six():\n    return 6\n";
    let a1 = a0
        .replace("x + 1", "x + 2")
        .replace("self.n = 1", "self.n = 2")
        .replace("return 1\n        return x", "return 11\n        return x")
        .replace("def retired(x):\n    return x\n\n\n", "")
        .replace("'nfkc'", "'NFKC'")
        .replace("'é'", "'É'")
        .replace("MAX_ITEMS = OTHER = 10", "MAX_ITEMS = OTHER = 11")
        .replace("LIMIT: int = 3", "LIMIT: int = 4")
        .replace("return 1\n\n\ndef beta", "return 10\n\n\ndef beta")
        .replace("return 2\n\n\ndef gamma", "return 20\n\n\ndef gamma")
        .replace("return 6\n\n\ndef delta", "return 60\n\n\ndef delta")
        .replace("def delta_six():\n    return 6\n", "def delta_six():\n    return 66\n");
    let common: Vec<File> = vec![
        f("callers.py", "from a import scale\n\n\ndef grow():\n    return scale(2) + Shape().area(1)\n"),
        f("square.py", "from a import Shape\n\n\nclass Square(Shape):\n    pass\n\nclass Cube(pkg.Shape, Mixin):\n    pass\n"),
        f("tests/test_a.py", "from a import scale\n\n\ndef test_scale():\n    assert scale(1) == 2\n"),
        f("foo_test.go", "func TestScale(t *testing.T) { scale(1) }\n"),
        f("src/contest.py", "x = scale(3)\n"),
        f("old_user.py", "from a import retired\n\nVALUE = retired(1)\n"),
        f("web/page.js", "const n = computeTotal([1, 2]);\n"),
        f("web/Cart.java", "public class Cart extends Base implements Api {\n  int computeTotal() { return 1; }\n}\n"),
        f("rs/lib.rs", "impl Display for Shape {\n}\nimpl<T> fmt::Debug for Shape {\n}\n"),
        f("dir with space/x [1].py", "y = scale(1)\n"),
        f("ünï/çødé.js", "computeTotal(1)\n"),
        f("usés_café.py", "z = café()\n"),
        f("a\"quote.py", "q = scale(9)\n"),
        f("tab\there.py", "t = scale(8)\n"),
        f("new\nline.py", "nl = scale(7)\n"),
        f("-dash.py", "d = scale(6)\n"),
        f(":colon.py", "c = scale(5)\n"),
        f("*star.py", "s = scale(4)\n"),
        f("back\\slash.py", "b = scale(3)\n"),
        f("big.py", &big("x")),
        f("r/a2.py", "alpha_one(); gamma_six(); delta_six()\n"),
        f("r/b1.py", "alpha_one(); beta_two()\n"),
        f("r/c3.py", "beta_two(); gamma_six(); delta_six()\n"),
        f("r/c4.py", "beta_two(); gamma_six(); delta_six()\n"),
        f("r/d5.py", "gamma_six(); delta_six()\n"),
        f("r/d6.py", "delta_six(); gamma_six()\n"),
        f("r/d7.py", "gamma_six(); delta_six()\n"),
        fb(b"latin_ref.py", "100644", b"scale(1)  # caf\xe9\n"),
        fb(b"bin_ref.dat", "100644", b"scale\x00(1)\n"),
        f("README.rst", "Read me (rst)\n"),
        f("uni.js", "x = 1\n"),
        f("README.md", "# Demo\n\nscale is documented here.\n"),
        f("crlf.txt.js", "function crlfThing() {\r\n  return 1;\r\n}\r\n"),
        f("ff.js", "function formFeed() {\n  return 1;\n}\n"),
        f("chmod.sh", "echo hi\n"),
        f("link_target.py", "def target_fn():\n    pass\n"),
        fb(b"type_change", "100644", b"plain file\n"),
        f("gone.py", "def gone_fn():\n    return scale(1)\n"),
        f("lib/old_name.py", "def renamed_fn(a):\n    return a\n"),
        f("lib/same.py", "def unchanged_rename():\n    return 0\n"),
        fb(b"image.png", "100644", b"\x89PNG\r\n\x1a\n\x00\x00binary"),
        fb(b"sub", "160000", gitlink_a.as_bytes()),
        f("rust/server.go", "func (s *Server) Handle(w http.ResponseWriter) {\n}\n"),
        f("notes/CHANGES.MD", "def not_a_symbol(): prose\n"),
        f("..md", "def dotdot_prose(): x\n"),
        f("prose.markdown", "class NotSymbol\n"),
    ];
    let mut f0 = vec![f("a.py", a0)];
    f0.extend(common.iter().cloned());
    let c0 = commit(&repo, &f0, &[]);

    let mut f1 = vec![f("a.py", &a1)];
    for file in &common {
        let p = String::from_utf8_lossy(&file.0).into_owned();
        match p.as_str() {
            "gone.py" | "lib/old_name.py" | "lib/same.py" | "type_change" | "sub" | "image.png" | "crlf.txt.js" | "ff.js"
            | "chmod.sh" | "rust/server.go" | "notes/CHANGES.MD" | "..md" | "prose.markdown" => {}
            _ => f1.push(file.clone()),
        }
    }
    f1.extend([
        f("lib/new_name.py", "def renamed_fn(a, b=1):\n    return a + b\n"),
        f("lib/moved.py", "def unchanged_rename():\n    return 0\n"),
        fb(b"type_change", "120000", b"a.py"),
        fb(b"symlink.py", "120000", b"link_target.py"),
        fb(b"sub", "160000", gitlink_b.as_bytes()),
        fb(b"image.png", "100644", b"\x89PNG\r\n\x1a\n\x00\x00binary2"),
        fb(b"new.bin", "100644", b"\x00\x01\x02"),
        fb(b"latin.py", "100644", b"def latin_fn():\n    return 'caf\xe9'\n"),
        f("crlf.txt.js", "function crlfThing() {\r\n  return 2;\r\n}\r\n"),
        f("ff.js", "function formFeed() {\n  return 2;\x0c@@ -1 +1 @@ function injectedName() {\n}\n"),
        f("uni.py", "x = 1\ny = \"\u{2028}@@ -\u{0661} +\u{0662},\u{0663} @@ def zzz():\"\nZED_CONST = 2\n"),
        f("uni2.js", "x = 1\ny = \"\u{2028}@@ -\u{0661} +\u{0662},\u{0663} @@ def zzz():\"\nconst ZED_JS = 2\n"),
        fb(b"chmod.sh", "100755", b"echo hi\n"),
        f("empty.py", ""),
        f("new\nchanged.py", "def nl_fn():\n    return scale(2)\n"),
        f("broken.py", "def broken(:\n    TOTAL_BROKEN = 1\nclass Half\n"),
        fb(b"nul.py", "100644", b"def nul_fn():\n    return '\x00'\n"),
        f("rust/server.go", "func (s *Server) Handle(w http.ResponseWriter) {\n  s.n++\n}\npub fn parse_line(s: &str) -> u8 {\npublic int countItems(List x) {\nreturn helper(x) {\n"),
        f("notes/CHANGES.MD", "def not_a_symbol(): prose 2\n"),
        f("..md", "def dotdot_prose(): y\n"),
        f("prose.markdown", "class NotSymbol2\n"),
        f("ünï/new [x].py", "def unicode_path_fn():\n    return scale(1)\n"),
    ]);
    let c1 = commit(&repo, &f1, &[&c0]);

    // README.md changed: README.rst is the README; then no README file at all, only a folder named README
    let mut f2 = f1.clone();
    f2.retain(|x| x.0 != b"README.md");
    f2.push(f("README.md", "# Demo 2\n"));
    let c2 = commit(&repo, &f2, &[&c1]);
    let mut f3 = f2.clone();
    f3.retain(|x| !x.0.starts_with(b"README"));
    f3.push(f("README/notes.txt", "a folder named README\n"));
    f3.push(fb(b"sub2", "160000", c0.as_bytes())); // a gitlink to a commit that is in this repository
    let c3 = commit(&repo, &f3, &[&c2]);
    // a side branch from c0 and a merge of it into c1
    let mut fs = f0.clone();
    fs.push(f("side.py", "def side_fn():\n    return scale(1)\n"));
    let s1 = commit(&repo, &fs, &[&c0]);
    let mut fm = f1.clone();
    fm.push(f("side.py", "def side_fn():\n    return scale(1)\n"));
    let m = commit(&repo, &fm, &[&c1, &s1]);
    (repo, vec![c0, c1, c2, c3, s1, m])
}

#[test]
fn edge_repository_material_is_identical() {
    let (repo, c) = edge_repo();
    let mut cases = budget_sweep(&repo, &c[0], &c[1], 6);
    for (b, h) in [(0, 1), (1, 0), (1, 2), (2, 3), (0, 0), (1, 5), (4, 5)] {
        cases.push((repo.clone(), c[b].clone(), c[h].clone(), Some("PR ☃\r\n".to_string()), context::BUDGET));
    }
    for budget in [20_000, 1] {
        cases.push((repo.clone(), c[0].clone(), c[1].clone(), None, budget));
    }
    let n = same_builds(&cases);
    let m = context::build_mode(&repo, &c[0], &c[1], None, context::BUDGET, "full", context::CHANGED_WINDOW).unwrap();
    // the cases this repository is for really occur
    for needle in [
        "=== sub (after the change) ===\n(submodule at commit ",
        "Changed submodules, shown only as their commit:\n- sub at ",
        "- big.py: lines 1-25, 485-535, 1485-1515, 2975-3000; refers to scale",
        "Left out as binary or not UTF-8:\n- latin_ref.py\n",
        "=== latin.py (after the change) ===\n(binary or not UTF-8 text; not shown)",
        "=== new\nchanged.py (after the change) ===\ndef nl_fn():",
        "=== dir with space/x [1].py (unchanged, at head) ===",
        "=== a\"quote.py (unchanged, at head) ===",
        "- square.py: whole file; subclasses or implements Shape",
    ] {
        assert!(m.contains(needle), "{needle:?} not in the material");
    }
    let searched = m.split("Changed symbols searched").nth(1).unwrap().split('\n').next().unwrap();
    for sym in ["injectedName", "zzz", "file_name", "café", "dotdot_prose", "broken", "Half", "TOTAL_BROKEN", "MAX_ITEMS", "retired"] {
        assert!(searched.contains(sym), "{sym} not searched: {searched}");
    }
    for sym in ["not_a_symbol", "NotSymbol"] {
        assert!(!searched.contains(sym), "{sym} searched: {searched}");
    }
    assert!(m.find("=== r/a2.py").unwrap() < m.find("=== r/b1.py").unwrap(), "an exact 5/6 tie goes by path");
    assert!(n > 15);
}

#[test]
fn module_functions_match_on_the_edge_repository() {
    let (repo, c) = edge_repo();
    let r = repo.to_str().unwrap();
    let pairs = context::changes(&repo, &c[0], &c[1]).unwrap();
    let pairs_json = json!(pairs);
    let (syms, owners) = context::symbols(&repo, &c[0], &c[1], &pairs, None).unwrap();
    let mut exclude: Vec<String> = pairs.iter().filter_map(|p| p.1.clone()).collect();
    exclude.push("README.md".into());
    let found = context::matches(&repo, &c[1], &syms, &owners, &exclude.iter().cloned().collect()).unwrap();
    let found_json: serde_json::Map<String, Value> = found
        .iter()
        .map(|(p, e)| (p.clone(), json!({"lines": e.lines, "why": e.why.iter().map(|(k, s)| json!([k, s])).collect::<Vec<_>>()})))
        .collect();
    let ranked: Vec<String> = context::rank(found.clone()).into_iter().map(|(p, _)| p).collect();
    let reads = [
        (c[1].as_str(), "a.py"), (c[1].as_str(), "new\nline.py"), (c[1].as_str(), "sub"), (c[1].as_str(), "lib"),
        (c[1].as_str(), "missing.py"), (c[3].as_str(), "sub2"), (c[1].as_str(), "symlink.py"), (c[1].as_str(), "ünï/new [x].py"),
        (c[1].as_str(), "a.py"),
    ];
    let mut objects = context::Objects::new(&repo);
    let rust_reads: Vec<Value> = reads
        .iter()
        .map(|(rev, p)| {
            let found = objects.read(rev, p).unwrap();
            json!(found.map(|(k, d)| json!([k, d.iter().map(|b| format!("{b:02x}")).collect::<String>()])))
        })
        .collect();
    let calls = [
        ("changes", json!([r, c[0], c[1]])),
        ("all_hunks", json!([r, c[0], c[1], pairs_json])),
        ("symbols", json!([r, c[0], c[1], pairs_json])),
        ("matches", json!([r, c[1], syms, owners, exclude])),
        ("rank", json!([found_json])),
        ("grep", json!([r, c[1], ["scale", "computeTotal", "café"]])),
        ("grep", json!([r, c[1], ["scale", "computeTotal"]])),
        ("grep", json!([r, c[1], ["nothing_matches_this"]])),
        ("objects_read", json!([r, reads])),
        ("tree_entry", json!([r, c[1], "sub"])),
        ("tree_entry", json!([r, c[1], "nope"])),
        ("tree_entry", json!([r, c[1], "new\nline.py"])),
        ("blob", json!([r, c[1], "latin.py"])),
        ("blob", json!([r, c[1], "nul.py"])),
        ("blob", json!([r, c[1], "lib"])),
        ("blob", json!([r, c[1], "nope.py"])),
        ("blob", json!([r, c[1], "new\nline.py"])),
        ("blob", json!([r, c[1], "ünï/new [x].py"])),
        ("complete", json!([r, [c[1], c[0]]])),
        ("commit", json!([r, &c[1][..10], "head"])),
    ];
    let want = oracle(&calls);
    let all = context::all_hunks(&repo, &c[0], &c[1], &pairs).unwrap();
    let grep = |names: &[&str]| res!(context::grep(&repo, &c[1], &names.iter().map(|s| s.to_string()).collect::<Vec<_>>()));
    let got = [
        res!(context::changes(&repo, &c[0], &c[1])),
        json!({"ok": all}),
        json!({"ok": [syms, owners]}),
        json!({"ok": found_json}),
        json!({"ok": ranked}),
        grep(&["scale", "computeTotal", "café"]),
        grep(&["scale", "computeTotal"]),
        grep(&["nothing_matches_this"]),
        json!({"ok": rust_reads}),
        res!(context::tree_entry(&repo, &c[1], "sub")),
        res!(context::tree_entry(&repo, &c[1], "nope")),
        res!(context::tree_entry(&repo, &c[1], "new\nline.py")),
        res!(context::blob(&repo, &c[1], "latin.py", None)),
        res!(context::blob(&repo, &c[1], "nul.py", None)),
        res!(context::blob(&repo, &c[1], "lib", None)),
        res!(context::blob(&repo, &c[1], "nope.py", None)),
        res!(context::blob(&repo, &c[1], "new\nline.py", None)),
        res!(context::blob(&repo, &c[1], "ünï/new [x].py", None)),
        res!(context::complete(&repo, &[&c[1], &c[0]])),
        res!(context::commit(&repo, &c[1][..10], "head")),
    ];
    for ((name, args), (g, w)) in calls.iter().zip(got.iter().zip(&want)) {
        assert_eq!(g, w, "{name}({args})");
    }
    // the -F -w fallback is what a git without PCRE runs: the same lines as -P
    let names = ["scale", "computeTotal"];
    let mut args = vec!["grep", "-n", "-I", "-z", "--no-color", "--no-column", "-w", "-F"];
    for n in names {
        args.extend(["-e", n]);
    }
    args.extend([c[1].as_str(), "--"]);
    assert_eq!(context::git_ok(&repo, &args, &[0, 1]).unwrap(), context::grep(&repo, &c[1], &names.map(String::from)).unwrap());
}

// ---------------------------------------------------------------------------------------------------------------
// Errors: the exact text of every refusal.

/// git refuses one PCRE alternation past about 31 KB, and the -F -w fallback ran for minutes on a 1,899-name merge:
/// thousands of names go in several -e patterns, with the same lines as Python, each once, and the same matches.
#[test]
fn thousands_of_names_grep_and_match_as_python_does() {
    let repo = init("many-names");
    let head = commit(&repo, &[f("a.py", "x = name_00000_symbol\ny = name_02599_symbol(name_01300_symbol)\nz = 1\n")], &[]);
    let names: Vec<String> = (0..2600).map(|i| format!("name_{i:05}_symbol")).collect();
    let r = repo.to_str().unwrap();
    let started = std::time::Instant::now();
    let out = context::grep(&repo, &head, &names).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
    let lines: Vec<&str> = out.split('\n').filter(|l| !l.is_empty()).map(|l| l.split('\0').nth(1).unwrap()).collect();
    assert_eq!(lines, ["1", "2"]);
    let found = context::matches(&repo, &head, &names, &[], &Default::default()).unwrap();
    let found_json: serde_json::Map<String, Value> = found
        .iter()
        .map(|(p, e)| (p.clone(), json!({"lines": e.lines, "why": e.why.iter().map(|(k, s)| json!([k, s])).collect::<Vec<_>>()})))
        .collect();
    let want = oracle(&[("grep", json!([r, head, names])), ("matches", json!([r, head, names, [], []]))]);
    assert_eq!(want[0], json!({"ok": out}));
    assert_eq!(want[1], json!({"ok": found_json}));
}

#[test]
fn errors_have_python_s_exact_text() {
    let (repo, base, head) = fixture(&[]);
    let mut cases = Vec::new();
    for bad in ["--output=/tmp/x", "", "nope", "0000000000000000000000000000000000000000", "it's", "a\"b'c", "tab\there",
                "zwsp\u{200b}", "nbsp\u{a0}", "emoji😀", "\u{e0001}tag", "-", "HEAD~999", "\u{7f}del"] {
        cases.push((repo.clone(), bad.to_string(), head.clone(), None, context::BUDGET));
        cases.push((repo.clone(), base.clone(), bad.to_string(), None, context::BUDGET));
    }
    let missing = repo.parent().unwrap().join("missing");
    for p in [missing.clone(), PathBuf::from(format!("{}//./", missing.display())), PathBuf::from("")] {
        cases.push((p, base.clone(), head.clone(), None, context::BUDGET));
    }
    cases.push((PathBuf::from(format!("{}/.//", repo.display())), base.clone(), head.clone(), None, 0));
    cases.push((repo.clone(), base.clone(), head.clone(), None, -5));
    // unrelated histories: no merge base
    let other = commit(&repo, &[f("x.py", "def x():\n    pass\n")], &[]);
    cases.push((repo.clone(), base.clone(), other.clone(), None, context::BUDGET));
    // a changed path that is not UTF-8: its text cannot be read back by name
    let r2 = init("nonutf8");
    let b2 = commit(&r2, &[f("a.py", "x = 1\n")], &[]);
    let h2 = commit(&r2, &[f("a.py", "x = 1\n"), fb(b"caf\xe9.py", "100644", b"y = 2\n")], &[&b2]);
    cases.push((r2.clone(), b2.clone(), h2.clone(), None, context::BUDGET));
    // a gitlink whose commit is not in the repository: git 2.54 answers '<id> submodule', which context.py's
    // batch reader indexes past (IndexError)
    let h3 = commit(&r2, &[f("a.py", "x = 1\n"), fb(b"sub", "160000", b"2222222222222222222222222222222222222222")], &[&b2]);
    cases.push((r2.clone(), b2.clone(), h3.clone(), None, context::BUDGET));
    // an incomplete partial clone is refused, and nothing is fetched
    g(&repo, &["config", "uploadpack.allowFilter", "true"], b"");
    g(&repo, &["update-ref", "refs/heads/main", &head], b"");
    g(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"], b"");
    let clone = tmp("partial").join("clone");
    let url = format!("file://{}", repo.display());
    let ok = Command::new("git")
        .args(["clone", "-q", "--filter=blob:none", "--no-checkout", &url])
        .arg(&clone)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    let before = g(&clone, &["count-objects", "-v"], b"");
    cases.push((clone.clone(), base.clone(), head.clone(), None, context::BUDGET));
    same_builds(&cases);
    assert_eq!(before, g(&clone, &["count-objects", "-v"], b""));
    let e = context::build(&clone, &base, &head, None, context::BUDGET).unwrap_err();
    assert!(e.msg.starts_with("the repository is an incomplete partial clone: "), "{}", e.msg);
    let e = context::build(&repo, "nope", &head, None, context::BUDGET).unwrap_err();
    assert_eq!((e.exc.name(), e.msg.as_str()), ("ContextError", format!("base 'nope' is not a commit in {}", repo.display()).as_str()));
}

#[test]
fn repr_matches_python_for_every_code_point() {
    let points: Vec<u32> = (0..0x110000u32).filter(|c| !(0xd800..=0xdfff).contains(c)).collect();
    let chunks: Vec<Vec<u32>> = points.chunks(64).map(|c| c.to_vec()).collect();
    let want = one("cp_repr", json!([chunks]));
    let got: Vec<String> = chunks
        .iter()
        .map(|c| {
            let rev: String = std::iter::once('-').chain(c.iter().map(|&p| char::from_u32(p).unwrap())).collect();
            context::commit(Path::new("."), &rev, "base").unwrap_err().msg
        })
        .collect();
    assert_eq!(json!({"ok": got}), want);
}

// ---------------------------------------------------------------------------------------------------------------
// Pure functions on many inputs, and Python's regex classes over every code point.

/// Python's `\s`, `\w`, `\b`, `\d` and int() against the Rust patterns, through public functions, on `points`.
fn regex_classes_match_python(points: &[u32]) {
    let chars = || points.iter().map(|&c| (c, char::from_u32(c).unwrap()));
    let space: Vec<u32> = chars().filter(|(_, c)| !context::text_symbols(&[], &[format!("{c}ABC = 1")]).is_empty()).map(|p| p.0).collect();
    let word: Vec<u32> = chars().filter(|(_, c)| context::is_test(&format!("a_test.{c}"))).map(|p| p.0).collect();
    let boundary: Vec<u32> = chars().filter(|(_, c)| context::kind(&format!("def abc{c}"), "abc") == "definition").map(|p| p.0).collect();
    let digit: Vec<Value> = chars()
        .filter_map(|(p, c)| {
            let (olds, ..) = context::hunks(Path::new("."), "", "", None, None, Some(&format!("@@ -{c} +1 @@"))).unwrap();
            (!olds.is_empty()).then(|| json!([p, olds]))
        })
        .collect();
    let want = oracle(&[("cp_space", json!([points])), ("cp_word", json!([points])), ("cp_boundary", json!([points])), ("cp_digit", json!([points]))]);
    assert_eq!(want[0], json!({"ok": space}), "\\s");
    assert_eq!(want[1], json!({"ok": word}), "\\w");
    assert_eq!(want[2], json!({"ok": boundary}), "\\b");
    assert_eq!(want[3], json!({"ok": digit}), "\\d and int()");
}

fn all_points() -> impl Iterator<Item = u32> {
    (0..0x110000u32).filter(|c| !(0xd800..=0xdfff).contains(c))
}

#[test]
fn regex_classes_match_python_on_a_code_point_sample() {
    // every code point below U+0800 and every 13th above, plus the ones where Python and Rust classes part ways
    let special = [0x1c, 0x1d, 0x1e, 0x1f, 0x85, 0xa0, 0xad, 0xb2, 0x2028, 0x2029, 0x200b, 0x200c, 0x200d, 0x203f, 0x2160, 0x2189,
        0x3007, 0x0301, 0xfb01, 0xff10, 0x1d7ce, 0x1d7ff, 0x1fbf0, 0xe0001, 0x10ffff];
    let points: Vec<u32> = all_points().filter(|&c| c < 0x800 || c % 13 == 0 || special.contains(&c)).collect();
    regex_classes_match_python(&points);
}

#[test]
#[ignore = "every code point (about a minute in a debug build)"]
fn regex_classes_match_python_for_every_code_point() {
    regex_classes_match_python(&all_points().collect::<Vec<_>>());
}

/// A small deterministic generator for random inputs.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n.max(1)
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.next(xs.len() as u64) as usize]
    }
}

const PIECES: &[&str] = &[
    "def ", "class ", "fn ", "func ", "struct ", "impl ", "for ", "extends ", "implements ", "object ", "export ", "const ",
    "static ", "type ", "(", ")", "(s *Server) ", "[T]", "<T>", "{", "}", ";", ":", "::", "=", "==", " ", "\t", "\u{1c}", "\u{a0}",
    "\u{2028}", "\r", "\n", "\x0c", "MAX_ITEMS", "Base", "Api", "abc", "abc_1", "café", "²", "x\u{301}", "‿", "_", "__",
    "return", "public int ", "Shape", "Mixin", "pkg.", ",", "*", "&", ".", "@@ -1,2 +3 @@ ", "-", "+", "Σ", "٣", "export\u{1f}",
    "\u{200d}", "日本",
];

fn random_lines(seed: u64, count: usize) -> Vec<String> {
    let mut r = Lcg(seed);
    (0..count).map(|_| (0..r.next(9) + 1).map(|_| r.pick(PIECES)).collect()).collect()
}

#[test]
fn pure_functions_match_on_many_inputs() {
    let lines = random_lines(7, 4000);
    let mut fixed: Vec<String> = [
        "func (s *Server) Handle(w http.ResponseWriter) {", "pub fn parse_line(s: &str) -> u8 {", "public int countItems(List x) {",
        "return helper(x) {", "MAX_ITEMS = 10", "x = 1", "export ABC_DEF: number = 3", "ABC == 3", "  static void main(String[] a) {",
        "class Sub(pkg.Base, Mixin):", "public class Sub extends Base implements Api {", "impl Display for Base {",
        "impl<T> std::fmt::Debug for Base {", "def Base(x):", "y = Base()", "object Foo: Base {", "struct S: Base {",
        "class Cube(pkg.Shape, Mixin):", "type Alias = Base", "Base = 1", "export Base: int = 2", "Base: int == 2",
        "def café(): pass", "class Base[T](Generic[T]):", "interface Api extends Base, Other {", "if (x) {",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    fixed.extend(lines.iter().cloned());
    let syms = ["Base", "Api", "Display", "abc", "café", "Shape", "MAX_ITEMS", "x\u{301}", "abc_1", "Mixin"];
    let pairs: Vec<(String, String)> =
        fixed.iter().enumerate().flat_map(|(i, l)| [(l.clone(), syms[i % syms.len()].to_string()), (l.clone(), "Base".to_string())]).collect();
    let paths: Vec<String> = [
        "pkg/tests/util.py", "foo_test.go", "src/contest.py", "test_x.py", "a/b/FooTests.java", "a/specs/x.rb", "x.spec.ts",
        "x_test.py\n", "__tests__/a.js", "testing", "a/test", "café_test.ç", "a.test.", "Test.java", "x_spec.", "./tests/x",
        "a//tests//b.py", "atest_x.py", "test_", "a/testing/b", "FooTest.Ⅻ",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(lines.iter().take(500).map(|l| format!("d/{l}"))) // a path with no parts is an IndexError in Python
    .collect();
    let mut r = Lcg(11);
    let texts: Vec<(String, Vec<u64>, i64)> = (0..300)
        .map(|_| {
            let n = r.next(120) as usize;
            let text: String = (0..n).map(|i| format!("{}{}", r.pick(&["row", "\u{c}", "é", "", "x\r"]), i) + r.pick(&["\n", "\r\n", "\n"])).collect();
            let text = if r.next(3) == 0 { text.trim_end().to_string() } else { text };
            let lines: Vec<u64> = (0..r.next(6)).map(|_| r.next(n as u64 + 20) + 1).collect();
            (text, lines, r.next(4000) as i64 - 100)
        })
        .collect();
    let diffs: Vec<String> = (0..300)
        .map(|_| {
            (0..r.next(12))
                .map(|_| {
                    r.pick(&["@@ -1,2 +3,4 @@ def f():", "@@ -5 +6 @@", "@@ -0,0 +1 @@ ctx", "-old", "+new", " ctx", "\\ No newline",
                        "@@ -١٢ +٣,0 @@", "@@ -7,1 +7,1 @@x", "diff --git a/x b/x", "+\u{2028}@@ -9 +9 @@ y", "@@ -3,0 +3,0 @@",
                        "@@-1 +1 @@", "+\u{c}-x", "-\r+y"])
                        .to_string()
                        + r.pick(&["\n", "\r\n", "\u{85}", "\u{b}"])
                })
                .collect()
        })
        .collect();
    let whys: Vec<Vec<(String, String)>> = (0..50)
        .map(|_| (0..r.next(6)).map(|_| (r.pick(&context::KIND_ORDER).to_string(), r.pick(&syms).to_string())).collect())
        .collect();
    let items: Vec<Vec<String>> = [0usize, 1, 199, 200, 201, 450].iter().map(|&n| (0..n).map(|i| format!("item {i}")).collect()).collect();

    let mut calls: Vec<(&str, Value)> = vec![
        ("text_symbols_each", json!([fixed])),
        ("text_symbols", json!([fixed[..10], fixed[10..]])),
        ("kind_each", json!([pairs])),
        ("is_test_each", json!([paths])),
    ];
    calls.extend(texts.iter().map(|(t, l, room)| ("windows", json!([t, l, room]))));
    calls.extend(diffs.iter().map(|d| ("hunks_text", json!([d]))));
    calls.extend(whys.iter().map(|w| ("describe", json!([w]))));
    calls.extend(items.iter().map(|i| ("listing", json!([i]))));
    let want = oracle(&calls);

    let mut got = vec![
        json!({"ok": fixed.iter().map(|l| context::text_symbols(&[], std::slice::from_ref(l))).collect::<Vec<_>>()}),
        json!({"ok": context::text_symbols(&fixed[..10], &fixed[10..])}),
        json!({"ok": pairs.iter().map(|(l, s)| context::kind(l, s)).collect::<Vec<_>>()}),
        json!({"ok": paths.iter().map(|p| context::is_test(p)).collect::<Vec<_>>()}),
    ];
    got.extend(texts.iter().map(|(t, l, room)| json!({"ok": context::windows(t, &l.iter().copied().collect(), *room)})));
    got.extend(diffs.iter().map(|d| res!(context::hunks(Path::new("."), "", "", None, None, Some(d)))));
    got.extend(whys.iter().map(|w| {
        let why: BTreeSet<(&'static str, String)> = w
            .iter()
            .map(|(k, s)| (*context::KIND_ORDER.iter().find(|x| *x == k).unwrap(), s.clone()))
            .collect();
        json!({"ok": context::describe(&why)})
    }));
    got.extend(items.iter().map(|i| json!({"ok": context::listing(i)})));
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "call {i}: {} {}", calls[i].0, calls[i].1.to_string().chars().take(300).collect::<String>());
    }
}

// ---------------------------------------------------------------------------------------------------------------
// python_symbols: the embedded copy is context.py's, its results and its exceptions are CPython's.

#[test]
fn embedded_python_symbols_is_context_py_s() {
    assert_eq!(one("getsource", json!([])), json!({"ok": context::PYTHON_SYMBOLS}));
    let src = "class Box:\n    SIZE = 2\n\n    def __init__(self):\n        self.n = 1\n\n    def open(self):\n        def helper():\n            return 1\n        return helper()\n";
    let cases: Vec<(&str, Vec<u64>)> = vec![
        (src, vec![5]), (src, vec![9]), (src, vec![10]), (src, vec![2]), (src, (1..=10).collect()), ("def (:", vec![1]),
        ("x = '\0'", vec![1]), ("\u{feff}x = 1\n", vec![1]), ("", vec![1]), ("A_B = ＭＡＸ = 1\n", vec![1]),
        ("@a\n@b(\n 1)\ndef f():\n    pass\n# trailing\n", vec![2]), ("x = 1\ry = 2\rZZZ = 3\r", vec![3]),
        ("class 𝔘𝔫𝔦𝔠𝔬𝔡𝔢:\n    def __eq__(s, o): pass\n", vec![2]), ("def f[T = int](): pass\n", vec![1]),
        ("x = t'{a}'\n", vec![1]), ("try:\n    pass\nexcept A, B:\n    pass\n", vec![1]), ("print 'x'\n", vec![1]),
    ];
    let calls: Vec<(&str, Value)> = cases.iter().map(|(s, l)| ("python_symbols", json!([s, l]))).collect();
    let want = oracle(&calls);
    for ((s, l), w) in cases.iter().zip(&want) {
        let got = res!(context::python_symbols(s, &l.iter().copied().collect()));
        assert_eq!(&got, w, "{s:?} {l:?}");
    }
}

#[test]
fn deep_sources_raise_where_the_python_runtime_raises() {
    // An elif chain nests one If per branch; walk() recursion fails near the interpreter's limit, at a depth that
    // counts the frames above python_symbols in `python3 ringo.py review`. Also the parser's own stack limit.
    let repo = init("deep");
    let base = commit(&repo, &[f("deep.py", "x = 0\n")], &[]);
    let mut cases = Vec::new();
    for n in (978..=992).chain([1500]) {
        let src = format!("if x:\n    pass\n{}", "elif x:\n    pass\n".repeat(n));
        let head = commit(&repo, &[f("deep.py", &src)], &[&base]);
        cases.push((repo.clone(), base.clone(), head, None, context::BUDGET));
    }
    let head = commit(&repo, &[f("deep.py", &format!("x = {}1\n", "-".repeat(100_000)))], &[&base]);
    cases.push((repo.clone(), base.clone(), head, None, context::BUDGET));
    let want = oracle(&cases.iter().map(|(r, b, h, p, n)| ("build", json!([r.to_str().unwrap(), b, h, p, n]))).collect::<Vec<_>>());
    let errs = |v: &[Value]| v.iter().map(|x| x.get("err").map(|e| e[0].clone())).collect::<Vec<_>>();
    assert!(errs(&want).iter().any(|e| e.is_none()) && errs(&want).iter().any(|e| e == &Some(json!("RecursionError"))));
    assert_eq!(errs(&want).last().unwrap(), &Some(json!("MemoryError")));
    same_builds(&cases);
}

fn corpus() -> Vec<PathBuf> {
    let stdlib = Command::new(ringo_cli::python())
        .args(["-c", "import sysconfig; print(sysconfig.get_paths()['stdlib'])"])
        .output()
        .unwrap();
    let mut files = Vec::new();
    let mut stack = vec![PathBuf::from(String::from_utf8(stdlib.stdout).unwrap().trim()), root()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            if name == ".git" || name == "target" || name == "site-packages" {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "py") {
                files.push(p);
            }
        }
    }
    files.sort();
    files
}

#[test]
#[ignore = "corpus sweep: every .py file of the Python standard library and of this repository (about a minute)"]
fn python_symbols_matches_cpython_on_the_stdlib_and_this_repository() {
    let mut cases: Vec<(String, Vec<u64>)> = Vec::new();
    let mut files = 0;
    for p in corpus() {
        let Ok(src) = std::fs::read_to_string(&p) else { continue };
        files += 1;
        let n = src.split('\n').count() as u64;
        let mut r = Lcg(files as u64);
        let mut sets: Vec<Vec<u64>> = vec![vec![r.next(n) + 1], (1..=n).collect()];
        for k in [3, 10, 25] {
            sets.push((0..k).map(|_| r.next(n) + 1).collect());
        }
        let a = r.next(n) + 1;
        sets.push((a..=(a + r.next(30)).min(n)).collect());
        for s in sets {
            cases.push((src.clone(), s));
        }
    }
    let mut disagreements = 0;
    for chunk in cases.chunks(400) {
        let want = oracle(&chunk.iter().map(|(s, l)| ("python_symbols", json!([s, l]))).collect::<Vec<_>>());
        let requests: Vec<(&str, BTreeSet<u64>)> = chunk.iter().map(|(s, l)| (s.as_str(), l.iter().copied().collect())).collect();
        let refs: Vec<(&str, &BTreeSet<u64>)> = requests.iter().map(|(s, l)| (*s, l)).collect();
        let got: Vec<Value> = match context::python_symbols_many(&refs) {
            Ok(all) => all.into_iter().map(|x| json!({"ok": x})).collect(),
            Err(_) => refs.iter().map(|(s, l)| res!(context::python_symbols(s, l))).collect(),
        };
        disagreements += got.iter().zip(&want).filter(|(g, w)| g != w).count();
    }
    println!("python_symbols corpus sweep: {files} files, {} cases, {disagreements} disagreements", cases.len());
    assert_eq!(disagreements, 0);
}

// ---------------------------------------------------------------------------------------------------------------
// Windowed and auto modes.

fn funcs(n: usize, tag: &str) -> String {
    (0..n).map(|i| format!("def f{i}(x):\n    return x + {i}  # {tag}\n\n")).collect()
}

fn js_funcs(n: usize, tag: &str) -> String {
    (0..n).map(|i| format!("function name_{i}(a) {{\n  return a + {i}; // {tag}\n}}\n\nconst LIMIT_{i} = {i};\n")).collect()
}

/// A repository whose base and head differ in large changed files (windows, merging, outlines past the cap, a
/// pure deletion, CRLF, non-Python definitions, added, deleted, renamed and mode-changed large files), lock and
/// generated files, long lines (multi-byte too), small files, a binary, a gitlink and an unchanged user of f100.
fn windowed_repo() -> (PathBuf, String, String) {
    let repo = init("windowed");
    let sub_a = commit(&repo, &[f("g.txt", "a\n")], &[]);
    let sub_b = commit(&repo, &[f("g.txt", "b\n")], &[]);
    let crlf = |t: String| t.replace('\n', "\r\n");
    let multi = |tag: &str| format!("x = 1\n{}\ny = 2  # {tag}\n{}", "é".repeat(1500), funcs(900, "m"));
    let user = format!("from a import f100\n\nV = f100(1)\n{}\n{}\n", "日".repeat(1400), "z".repeat(3000));
    let base_files = vec![
        f("a.py", &funcs(900, "a")),
        f("b.py", &funcs(900, "b")),
        f("c.py", &funcs(900, "c")),
        f("web/app.js", &js_funcs(700, "a")),
        f("crlf.py", &crlf(funcs(900, "a"))),
        f("multi.py", &multi("a")),
        f("rename_me.py", &funcs(900, "r")),
        f("rename_same.py", &funcs(900, "s")),
        f("gone.py", &funcs(900, "g")),
        f("modeonly.py", &funcs(900, "o")),
        f("small.py", "def tiny(x):\n    return x\n"),
        f("Cargo.lock", &"v1\n".repeat(30)),
        f("vendor/go.sum", &"h1\n".repeat(30)),
        f("gen.py", "# @generated by tool\nX = 1\n"),
        f("gen2.c", "/* a\n * b\n * DO NOT EDIT\n */\nint x;\n"),
        f("late.py", "a\nb\nc\nd\ne\n# @generated\nX = 1\n"),
        f("app.min.js", "var a=1;\n"),
        f("app.js.map", "{\"v\":1}\n"),
        f("yarn.lock", &"y1\n".repeat(40)),
        f("link.txt", "plain\n"),
        f("odd dir/Cargo.lock", "old\n"),
        f("marked.py", "# @generated\nX = 1\n"),
        f("newmark.py", "X = 1\n"),
        f("user.py", &user),
        f("README.md", "# Big\n"),
        fb(b"image.png", "100644", b"\x89PNG\x00\x00one"),
        fb(b"sub", "160000", sub_a.as_bytes()),
    ];
    let base = commit(&repo, &base_files, &[]);
    let edit = |text: &str, pairs: &[(&str, &str)]| pairs.iter().fold(text.to_string(), |t, (a, b)| t.replace(a, b));
    let a1 = edit(&funcs(900, "a"), &[("return x + 100 ", "return x + 1000 "), ("return x + 110 ", "return x + 1100 "), ("return x + 500 ", "return x + 5000 "),
        ("def f300(x):\n    return x + 300  # a\n\n", ""), ("return x + 899 ", "return x + 8990 ")]);
    let long_row = "Q".repeat(2500);
    let b1 = edit(&funcs(900, "b"), &[("return x + 100 ", &format!("return x + 1000 # {long_row} ")), ("return x + 3 ", "return x + 33 ")]);
    let c1 = format!("{}{}", funcs(900, "c"), "def appended(x):\n    return x\n"); // an end-of-file edit
    let c1 = c1.replacen("def f0(x):\n    return x + 0  # c\n\n", "", 1); // and a deletion at the very start
    let mut head_files: Vec<File> = vec![
        f("a.py", &a1),
        f("b.py", &b1),
        f("c.py", &c1),
        f("web/app.js", &edit(&js_funcs(700, "a"), &[("return a + 350; // a", "return a + 351; // a"), ("const LIMIT_10 = 10;", "const LIMIT_10 = 11;")])),
        f("crlf.py", &crlf(edit(&funcs(900, "a"), &[("return x + 400 ", "return x + 4000 ")]))),
        f("multi.py", &multi("b")),
        f("rename_new.py", &edit(&funcs(900, "r"), &[("return x + 5 ", "return x + 55 ")])),
        f("rename_same2.py", &funcs(900, "s")),
        fb(b"modeonly.py", "100755", funcs(900, "o").as_bytes()),
        f("added_big.py", &funcs(900, "n")),
        f("small.py", "def tiny(x):\n    return x + 1\n"),
        f("Cargo.lock", &format!("v2\n{}", "v1\n".repeat(30))),
        f("vendor/go.sum", &"h2\n".repeat(31)),
        f("gen.py", "# @generated by tool\nX = 2\n"),
        f("gen2.c", "/* a\n * b\n * DO NOT EDIT\n */\nint y;\n"),
        f("late.py", "a\nb\nc\nd\ne\n# @generated\nX = 2\n"),
        f("app.min.js", "var a=2;\n"),
        f("app.js.map", "{\"v\":2}\n"),
        fb(b"link.txt", "120000", b"a.py"),
        f("odd dir/Cargo.lock", "new\n"),
        f("marked.py", "# @generated\nX = 2\n"),
        f("newmark.py", "# DO NOT EDIT\nX = 2\n"),
        f("user.py", &user),
        f("README.md", "# Big\n"),
        fb(b"image.png", "100644", b"\x89PNG\x00\x00two"),
        fb(b"sub", "160000", sub_b.as_bytes()),
        f("empty.py", ""),
    ];
    head_files.retain(|x| x.0 != b"gone.py");
    let head = commit(&repo, &head_files, &[&base]);
    (repo, base, head)
}

/// Every mode and window over a spread of budgets, with PR texts: windowed and auto from the change alone's size
/// (windowed and refilled) up to the full material, so every refill step is the last one that fits somewhere.
fn mode_sweep(repo: &Path, base: &str, head: &str, steps: i64) -> usize {
    let need = change_alone(repo, base, head);
    let windowed = |w: i64| context::build_mode(repo, base, head, None, 1, "windowed", w).unwrap().split("\n=== context manifest ===").next().unwrap().len() as i64;
    let (small, mid) = (windowed(25), windowed(150));
    let mut budgets = vec![context::BUDGET, need + 20_000, need, need - 1, need - 100, small, small - 1, mid, mid + 1, small + 700, 10, 1];
    budgets.extend((0..=steps).map(|k| small - 50 + k * ((need - small + 100) / steps).max(1)));
    let mut total = 0;
    for (mode, window) in [("windowed", 25), ("windowed", 0), ("auto", 25), ("auto", 60)] {
        let mut cases = Vec::new();
        for (i, &budget) in budgets.iter().enumerate() {
            if budget > 0 {
                cases.push((repo.to_path_buf(), base.to_string(), head.to_string(), PRS[i % PRS.len()].map(str::to_string), budget));
            }
        }
        total += same_builds_mode(&cases, Some((mode, window)));
    }
    let mut cases = vec![(repo.to_path_buf(), base.to_string(), head.to_string(), None, context::BUDGET)];
    cases.extend(budgets.iter().filter(|&&b| b > 0).map(|&b| (repo.to_path_buf(), base.to_string(), head.to_string(), Some("PR".into()), b)));
    total + same_builds_mode(&cases, None)
}

#[test]
fn windowed_and_auto_material_is_identical_on_large_changed_files() {
    let (repo, base, head) = windowed_repo();
    let w = context::build_mode(&repo, &base, &head, None, context::BUDGET, "windowed", context::CHANGED_WINDOW).unwrap();
    // the cases this repository is for really occur
    for needle in [
        "- a.py: lines 277-357, 876-926, 1474-1524, 2671-2697 of 2697",
        "=== Cargo.lock (generated or lock file; not shown) ===\n+1 -0 lines\n",
        "diff --git a/Cargo.lock b/Cargo.lock\n(generated or lock file: +1 -0 lines; not shown)\n",
        "=== vendor/go.sum (generated or lock file; not shown) ===\n",
        "=== gen2.c (generated or lock file; not shown) ===\n",
        "=== app.js.map (generated or lock file; not shown) ===\n",
        "=== late.py (after the change) ===\na\nb\n", // @generated past line 5 is not a marker
        "=== newmark.py (after the change) ===\n# DO NOT EDIT\nX = 2\n", // a marker the change adds does not classify
        "=== marked.py (generated or lock file; not shown) ===\n+1 -1 lines\n",
        "+X = 2\n", // marker-only: the diff stays
        "diff --git a/yarn.lock b/yarn.lock\n(generated or lock file: +0 -40 lines; not shown)\n", // a deleted lock file
        "- odd dir/Cargo.lock: generated or lock file; diff kept (could not be matched)\n",
        "- marked.py: +1 -1 lines; file block shown as a stat line (marked generated); diff kept\n",
        "diff --git a/Cargo.lock b/Cargo.lock\n(generated", // cut although link.txt is a type change
        "=== small.py (after the change) ===\ndef tiny(x):\n    return x + 1\n",
        "- rename_same2.py: no changed lines of 2700\n",
        "--- outline ---\n1: def f0(x):\n",
        "... and ",
        " \u{2026}[1527 more characters cut]\n",
        "(windowed: 2697 lines at head; shown: \u{b1}25 lines",
        "Changed files sent as windows:\n- a.py: lines ",
        "(binary or not UTF-8 text; not shown)",
        "Changed submodules, shown only as their commit:",
    ] {
        assert!(w.contains(needle), "{needle:?} not in the windowed material");
    }
    let b_block = w.split("=== b.py (after the change) ===\n").nth(1).unwrap().split("\n=== ").next().unwrap();
    assert!(!b_block.contains(&"Q".repeat(1001)) && w.contains(&"Q".repeat(2500)), "cut in the window, whole in the diff");
    assert!(w.contains(&"日".repeat(1000)) && !w.contains(&"日".repeat(1001)));
    let n = mode_sweep(&repo, &base, &head, 8);
    assert!(n > 80, "{n}");
}

#[test]
fn auto_refill_is_identical_where_it_stops_at_every_step() {
    // two large changed files: at the budgets below, refill ends with both at 60, both at 150, one whole, both whole
    let repo = init("refill");
    let files = |tag: &str| vec![f("a.py", &funcs(900, tag)), f("b.py", &funcs(900, tag)), f("README.md", "# R\n")];
    let base = commit(&repo, &files("a"), &[]);
    let head_files = vec![
        f("a.py", &funcs(900, "a").replace("return x + 100 ", "return x + 1000 ")),
        f("b.py", &funcs(900, "a").replace("return x + 100 ", "return x + 1000 ")),
        f("README.md", "# R\n"),
    ];
    let head = commit(&repo, &head_files, &[&base]);
    let need = change_alone(&repo, &base, &head);
    let size = |w: i64| context::build_mode(&repo, &base, &head, None, 1, "windowed", w).unwrap().len() as i64;
    let budgets = [size(25) + 10, (size(60) + size(150)) / 2, size(150) + 400, need - 100, need - 1, need];
    let cases: Vec<_> = budgets.iter().map(|&b| (repo.clone(), base.clone(), head.clone(), None, b)).collect();
    same_builds_mode(&cases, None);
    // how many changed files end at each width: (25, 60, 150, whole); a file that reached whole is no longer windowed
    let seen: Vec<[usize; 4]> = budgets
        .iter()
        .map(|&b| {
            let m = context::build(&repo, &base, &head, None, b).unwrap();
            let n = |w: &str| m.matches(&format!("shown: \u{b1}{w} lines")).count();
            [n("25"), n("60"), n("150"), 2 - m.matches("(windowed:").count()]
        })
        .collect();
    assert_eq!(seen, [[2, 0, 0, 0], [0, 1, 1, 0], [0, 0, 2, 0], [0, 0, 1, 1], [0, 0, 1, 1], [0, 0, 0, 2]], "{seen:?}");
}

#[test]
fn windowed_and_auto_material_is_identical_on_the_fixture_and_edge_repositories() {
    let (repo, base, head) = fixture(&[]);
    let (r2, c) = edge_repo();
    let mut total = 0;
    for (mode, window) in [("windowed", 25), ("auto", 25), ("auto", 3)] {
        let mut cases = budget_sweep(&repo, &base, &head, 20);
        cases.extend(budget_sweep(&r2, &c[0], &c[1], 12));
        for (b, h) in [(0, 1), (1, 0), (1, 2), (2, 3), (0, 0), (1, 5), (4, 5)] {
            cases.push((r2.clone(), c[b].clone(), c[h].clone(), Some("PR ☃\r\n".to_string()), context::BUDGET));
            cases.push((r2.clone(), c[b].clone(), c[h].clone(), None, 5_000));
        }
        total += same_builds_mode(&cases, Some((mode, window)));
    }
    assert!(total > 100);
}

#[test]
fn a_window_as_wide_as_i64_max_covers_the_file_as_python_does() {
    assert_eq!(context::spans_around(10, &BTreeSet::from([5]), i64::MAX), vec![(1, 10)]);
    let repo = init("wide");
    let base = commit(&repo, &[f("a.py", &funcs(900, "a")), f("README.md", "# R\n")], &[]);
    let head = commit(&repo, &[f("a.py", &funcs(900, "a").replace("return x + 100 ", "return x + 1000 ")), f("README.md", "# R\n")], &[&base]);
    let mut cases = Vec::new();
    for budget in [context::BUDGET, 1000, 20_000] {
        cases.push((repo.clone(), base.clone(), head.clone(), None, budget));
    }
    for (mode, window) in [("windowed", i64::MAX), ("auto", i64::MAX), ("windowed", i64::MAX - 1), ("windowed", 1 << 62)] {
        same_builds_mode(&cases, Some((mode, window)));
    }
}

#[test]
fn windowing_never_sends_more_than_full_and_auto_returns_the_smaller_when_neither_fits() {
    let repo = init("never-larger");
    let base = commit(&repo, &[f("a.py", &funcs(900, "a")), f("README.md", "# R\n")], &[]);
    let head = commit(&repo, &[f("a.py", &funcs(900, "a")), f("added.py", &funcs(900, "n")), f("README.md", "# R\n")], &[&base]);
    let w = context::build_mode(&repo, &base, &head, None, context::BUDGET, "windowed", 25).unwrap();
    assert!(w.contains("=== added.py (after the change) ===\ndef f0(x):\n    return x + 0  # n\n") && !w.contains("added.py: lines"));
    let cases: Vec<_> = [1000, 20_000, context::BUDGET].iter().map(|&b| (repo.clone(), base.clone(), head.clone(), None, b)).collect();
    same_builds_mode(&cases, None);
    same_builds_mode(&cases, Some(("windowed", 25)));
    let size = |mode: &str| context::build_mode(&repo, &base, &head, None, 1000, mode, 25).unwrap().len();
    assert_eq!(context::build(&repo, &base, &head, None, 1000).unwrap().len(), size("full").min(size("windowed")));
}

#[test]
fn mode_errors_have_python_s_exact_text() {
    let (repo, base, head) = fixture(&[]);
    let r = repo.to_str().unwrap();
    let bad: Vec<(Value, i64)> = vec![(json!("wide"), 25), (json!(""), 25), (json!("AUTO"), 25), (json!("full"), -1), (json!("auto"), -7), (json!("it's"), 25)];
    let calls: Vec<(&str, Value)> = bad.iter().map(|(m, w)| ("build", json!([r, base, head, null, context::BUDGET, m, w]))).collect();
    let want = oracle(&calls);
    for ((m, w), want) in bad.iter().zip(&want) {
        let got = rust_build(&repo, &base, &head, None, context::BUDGET, Some((m.as_str().unwrap(), *w)));
        assert_eq!(&got, want, "{m} {w}");
        assert!(got.get("err").is_some());
    }
}

#[test]
fn windowed_helpers_match_on_many_inputs() {
    let mut r = Lcg(5);
    let words = ["é", "日", "x", "\u{1f600}", "ab", " "];
    let long: Vec<String> = [0usize, 1, 999, 1000, 1001, 1002, 2500]
        .iter()
        .flat_map(|&n| words.iter().map(move |w| w.repeat(n)))
        .chain((0..60).map(|_| (0..r.next(1100) as usize).map(|_| words[r.next(6) as usize]).collect::<String>()))
        .collect();
    let named: Vec<(&str, &str)> = vec![
        ("Cargo.lock", ""), ("a/b/yarn.lock", "x"), ("x.min.js", ""), ("x.min.css", ""), ("a.map", ""), ("x.min.json", ""), ("dir/Cargo.lock.bak", ""), ("go.sum", ""),
        ("a.py", "# @generated"), ("a.py", "a\nb\nc\nd\n// DO NOT EDIT\n"), ("a.py", "a\nb\nc\nd\ne\nDO NOT EDIT\n"), ("a.py", "do not edit\n"), ("a.rs", "x @generated y\n"),
        ("flake.lock", "x"), ("pnpm-lock.yaml", ""), ("sub\n/Gemfile.lock", ""), ("./uv.lock", ""), ("x.MAP", ""),
    ];
    let mut paths_texts: Vec<(String, Option<String>, Option<String>, bool)> = Vec::new();
    for (p, t) in &named {
        for base in [None, Some(t.to_string()), Some("plain\n".to_string()), Some("# @generated\n".to_string())] {
            for deleted in [false, true] {
                paths_texts.push((p.to_string(), Some(t.to_string()), base.clone(), deleted));
            }
        }
        paths_texts.push((p.to_string(), None, Some(t.to_string()), false));
    }
    // replace_generated: changes (old, new), a diff of some of their headers (a type change prints two), stats for some
    let pairs: Vec<(Option<&str>, Option<&str>)> = vec![
        (Some("a.py"), Some("a.py")), (None, Some("n.py")), (Some("o.py"), None), (Some("old.py"), Some("new.py")),
        (Some("t.lock"), Some("t.lock")), (Some("odd dir/x"), Some("odd dir/x")), (Some("y.lock"), None), (Some("a.py"), Some("a.py")),
    ];
    let headers = ["diff --git a/a.py b/a.py", "diff --git a/n.py b/n.py", "diff --git a/o.py b/o.py", "diff --git a/old.py b/new.py",
        "diff --git a/t.lock b/t.lock", "diff --git a/odd dir/x b/odd dir/x", "diff --git a/y.lock b/y.lock", "diff --git a/zzz b/zzz"];
    let mut replaces: Vec<(String, Vec<(usize, (usize, usize))>)> = Vec::new();
    {
        let mut q = Lcg(21);
        for _ in 0..120 {
            let mut diff = if q.next(4) == 0 { "preamble\n".to_string() } else { String::new() };
            for _ in 0..q.next(9) {
                diff += &format!("{}\nindex 1..2\n+x\n", headers[q.next(headers.len() as u64) as usize]);
            }
            let mut stats: Vec<(usize, (usize, usize))> = Vec::new();
            for i in 0..pairs.len() {
                if q.next(2) == 0 {
                    stats.push((i, (q.next(9) as usize, q.next(9) as usize)));
                }
            }
            replaces.push((diff, stats));
        }
    }
    let diffs: Vec<String> = (0..200)
        .map(|_| {
            (0..r.next(8))
                .map(|_| r.pick(&["@@ -1,2 +3,4 @@ def f():", "@@ -5 +6 @@", "@@ -0,0 +1 @@ ctx", "@@ -4,3 +9,0 @@", "@@ -4 +0,0 @@", "@@ -١٢ +٣,٠ @@", "-old", "+new", " ctx", "@@ -2 +7,2 @@x"]).to_string() + "\n")
                .collect()
        })
        .collect();
    let texts: Vec<String> = (0..120)
        .map(|_| {
            let n = r.next(90) as usize;
            let text: String = (0..n).map(|_| format!("{}{}", r.pick(&["row", "def f():", "class A:", "    async def g():", "int main(int a) {", "ABC = 1", "\u{c}", "é", "x\r", "fn z() {", "export const AB_C = 2;", ""]), r.pick(&["\n", "\r\n"]))).collect();
            if r.next(3) == 0 { text.trim_end().to_string() } else { text }
        })
        .collect();
    let mut cases: Vec<(String, Vec<u64>, i64)> = Vec::new(); // (path, points, width)
    for i in 0..texts.len() {
        let n = texts[i].split('\n').count() as u64;
        cases.push((if i % 2 == 0 { "m.py" } else { "m.js" }.to_string(), (0..r.next(5)).map(|_| r.next(n + 6)).collect(), r.next(12) as i64));
    }
    let mut calls: Vec<(&str, Value)> = vec![
        ("cut_line_each", json!([long])),
        ("generated_kind_each", json!([paths_texts])),
    ];
    calls.extend(replaces.iter().map(|(d, st)| ("replace_generated", json!([d, pairs, st.iter().map(|(i, (a, b))| json!([i, [a, b]])).collect::<Vec<_>>()]))));
    calls.extend(diffs.iter().map(|d| ("new_points", json!([d]))));
    calls.extend(texts.iter().map(|t| ("line_rows", json!([t]))));
    calls.extend(texts.iter().map(|t| ("cut_long_lines", json!([t]))));
    let rows_of = |t: &String| context::line_rows(t).into_iter().map(context::cut_line).collect::<Vec<_>>();
    calls.extend(texts.iter().zip(&cases).map(|(t, (p, pts, w))| ("changed_windows", json!([p, rows_of(t), pts, w]))));
    let want = oracle(&calls);
    let mut got = vec![
        json!({"ok": long.iter().map(|l| context::cut_line(l)).collect::<Vec<_>>()}),
        json!({"ok": paths_texts.iter().map(|(p, t, b, d)| context::generated_kind(p, t.as_deref(), b.as_deref(), *d)).collect::<Vec<_>>()}),
    ];
    let rust_pairs: Vec<context::Pair> = pairs.iter().map(|(o, n)| (o.map(str::to_string), n.map(str::to_string))).collect();
    got.extend(replaces.iter().map(|(d, st)| {
        let (out, matched) = context::replace_generated(d, &rust_pairs, &st.iter().copied().collect());
        json!({"ok": [out, matched]})
    }));
    got.extend(diffs.iter().map(|d| json!({"ok": context::new_points(d)})));
    got.extend(texts.iter().map(|t| json!({"ok": context::line_rows(t)})));
    got.extend(texts.iter().map(|t| json!({"ok": context::cut_long_lines(t)})));
    got.extend(texts.iter().zip(&cases).map(|(t, (p, pts, w))| json!({"ok": context::changed_windows(p, &rows_of(t), &pts.iter().copied().collect(), *w)})));
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "call {i}: {} {}", calls[i].0, calls[i].1.to_string().chars().take(300).collect::<String>());
    }
}

// ---------------------------------------------------------------------------------------------------------------
// This repository's own history.

fn history() -> (PathBuf, Vec<(String, Vec<String>)>) {
    let repo = root();
    let out = Command::new("git").arg("-C").arg(&repo).args(["rev-list", "--parents", "--max-count=150", "--all"]).output().unwrap();
    let commits = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let mut ids = l.split(' ').map(str::to_string);
            (ids.next().unwrap(), ids.collect())
        })
        .collect();
    (repo, commits)
}

fn history_cases(limit: usize, budgets: &[i64]) -> Vec<(PathBuf, String, String, Option<String>, i64)> {
    let (repo, commits) = history();
    let mut cases = Vec::new();
    let mut add = |b: &str, h: &str, pr: Option<&str>| {
        for &budget in budgets {
            cases.push((repo.clone(), b.to_string(), h.to_string(), pr.map(str::to_string), budget));
        }
    };
    let mut merges = 0;
    for (i, (c, parents)) in commits.iter().take(limit).enumerate() {
        let pr = PRS[i % PRS.len()];
        for (k, p) in parents.iter().enumerate() {
            if k == 0 || merges < limit / 10 + 1 {
                add(p, c, pr);
            }
        }
        if parents.len() > 1 {
            merges += 1;
        }
        if let Some((far, _)) = commits.get(i + 7).filter(|_| i % 5 == 0) {
            add(far, c, None); // a multi-commit range (three-dot from an older commit)
        }
    }
    cases
}

#[test]
fn history_subset_is_identical() {
    // the first few small commits of HEAD's history (and a merge), so a debug build stays quick
    let repo = root();
    let out = Command::new("git").arg("-C").arg(&repo).args(["rev-list", "--parents", "--max-count=60", "HEAD"]).output().unwrap();
    let small = |b: &str, h: &str| {
        let stat = Command::new("git").arg("-C").arg(&repo).args(["diff", "--numstat", &format!("{b}...{h}")]).output().unwrap();
        let lines: i64 = String::from_utf8_lossy(&stat.stdout)
            .lines()
            .map(|l| l.split('\t').take(2).map(|n| n.parse::<i64>().unwrap_or(1000)).sum::<i64>())
            .sum();
        lines <= 150
    };
    let mut cases = Vec::new();
    let (mut plain, mut merges) = (0, 0);
    for line in String::from_utf8(out.stdout).unwrap().lines() {
        let ids: Vec<&str> = line.split(' ').collect();
        let (c, parents) = (ids[0], &ids[1..]);
        let wanted = if parents.len() > 1 { merges < 1 } else { plain < 3 };
        let sides: Vec<&&str> = parents.iter().filter(|p| small(p, c)).collect();
        if !wanted || sides.is_empty() {
            continue;
        }
        for (k, p) in sides.iter().enumerate() {
            let pr = PRS[(plain + merges + k) % PRS.len()].map(str::to_string);
            cases.push((repo.clone(), p.to_string(), c.to_string(), pr, if k == 0 { context::BUDGET } else { 3000 }));
        }
        if parents.len() > 1 { merges += 1 } else { plain += 1 }
    }
    assert!(merges == 1 && plain == 3, "{merges} merge(s), {plain} commit(s): this test needs the repository's \
             history (a full clone, or fetch-depth: 0 in CI)");
    same_builds(&cases);
    // the same commits in windowed and auto modes: this repository's changed files are mostly over 16 KB
    same_builds_mode(&cases, Some(("windowed", context::CHANGED_WINDOW)));
    same_builds_mode(&cases, None);
}

#[test]
#[ignore = "denser budget sweeps of the fixture and the edge repository (a few minutes)"]
fn budget_sweeps_are_identical() {
    let (repo, base, head) = fixture(&[]);
    let mut cases = budget_sweep(&repo, &base, &head, 400);
    let (r2, c) = edge_repo();
    for (b, h) in [(0, 1), (1, 0), (0, 5), (4, 1)] {
        cases.extend(budget_sweep(&r2, &c[b], &c[h], 60));
    }
    for (b, h) in [(0, 3), (3, 4), (2, 3)] {
        for budget in [context::BUDGET, 60_000, 20_000, 5_000, 1] {
            cases.push((r2.clone(), c[b].clone(), c[h].clone(), None, budget));
        }
    }
    let n = same_builds(&cases);
    println!("budget sweeps: {n} builds identical");
}

#[test]
#[ignore = "full-history sweep: every commit pair of the last 150 commits at three budgets (several minutes)"]
fn history_is_identical() {
    let cases = history_cases(150, &[context::BUDGET, 40_000, 3000]);
    let n = same_builds(&cases)
        + same_builds_mode(&cases, Some(("windowed", context::CHANGED_WINDOW)))
        + same_builds_mode(&cases, None);
    println!("history sweep: {n} builds identical");
}
