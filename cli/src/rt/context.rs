//! Ported from `context.py`: review material from a git repository, the change plus the unchanged code it depends
//! on. Deterministic (the same commits and budget give the same bytes), read-only (git object reads only) and
//! offline (GIT_NO_LAZY_FETCH=1; an incomplete partial clone is refused).
//!
//! Material, in order: the PR description, the three-dot diff, every changed file at head, the README at head, the
//! '=== <path> (unchanged, at head) ===' context blocks, then the '=== context manifest ==='.
//!
//! `python_symbols` runs CPython's `ast` through `python3` (one process per `build`, for every Python file of the
//! change): neither rustpython-parser nor tree-sitter-python gave CPython's names and owners on the standard
//! library, and the design review allows a native parser only with zero disagreements.

#![cfg_attr(not(unix), allow(unused_imports, dead_code))]

use crate::rt::{py, Error, Exc, Result};
use fancy_regex::Regex as Fancy;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::LazyLock;

/// Bytes of material, including the change itself and the manifest.
pub const BUDGET: i64 = 400_000;
/// An unchanged file up to this size goes in whole; a larger one as windows around matches.
pub const WHOLE_FILE: i64 = 16_000;
/// Lines of context above and below each match in a windowed file.
pub const WINDOW: i64 = 15;
/// Manifest lines per list before it summarizes the rest.
pub const LISTED: usize = 200;
pub const READMES: [&str; 4] = ["README.md", "README.rst", "README.txt", "README"];
/// Changed prose names no symbols ('the function above...').
pub const PROSE: [&str; 5] = [".md", ".markdown", ".rst", ".txt", ".adoc"];
/// GIT_LITERAL_PATHSPECS: a changed path such as 'pages/[id].tsx' or ':x' names that file, never a glob or magic.
pub const ENV: [(&str, &str); 4] =
    [("GIT_NO_LAZY_FETCH", "1"), ("GIT_TERMINAL_PROMPT", "0"), ("GIT_OPTIONAL_LOCKS", "0"), ("GIT_LITERAL_PATHSPECS", "1")];

pub const IDENT: &str = "[A-Za-z_][A-Za-z0-9_]*";
pub const KEYWORDS: &[&str] = &[
    "return", "new", "else", "throw", "await", "yield", "case", "delete", "typeof", "if", "while", "for", "switch",
    "catch", "do", "goto", "sizeof", "not", "and", "or", "in", "is", "elif", "except", "with",
];
/// KEYWORDS plus these.
const MORE_COMMON: &[&str] = &[
    "self", "cls", "this", "super", "main", "init", "test", "setup", "run", "get", "set", "None", "True", "False",
    "null", "nil", "true", "false", "object", "string", "value", "data", "name", "type", "args", "kwargs", "result",
    "error", "list", "dict", "str", "int", "bool", "void", "const", "static", "public", "private", "protected", "def",
    "class", "function", "struct", "impl", "pub", "async", "var", "let", "val", "export", "default", "import", "from",
];
pub static COMMON: LazyLock<BTreeSet<&'static str>> =
    LazyLock::new(|| KEYWORDS.iter().chain(MORE_COMMON).copied().collect());
pub const KIND_ORDER: [&str; 4] = ["definition", "subclass", "reference", "test"];


/// A Python `re` pattern (str, no flags) in Rust syntax: `\w`, `\s` and `\b` are Python's (`\w` is str.isalnum()
/// or '_', `\s` is str.isspace(), which also counts U+001C..U+001F); everything else is passed through.
fn py_re(pattern: &str) -> String {
    use py::{SPACE as S, WORD as W};
    let boundary = format!("(?:(?<=[{W}])(?![{W}])|(?<![{W}])(?=[{W}]))");
    let mut out = String::new();
    let mut class = false;
    let mut it = pattern.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => match (it.next(), class) {
                (Some('w'), false) => out.push_str(&format!("[{W}]")),
                (Some('w'), true) => out.push_str(W),
                (Some('s'), false) => out.push_str(&format!("[{S}]")),
                (Some('s'), true) => out.push_str(S),
                (Some('b'), false) => out.push_str(&boundary),
                (Some(n), _) => {
                    out.push('\\');
                    out.push(n);
                }
                (None, _) => out.push('\\'),
            },
            '[' if !class => {
                class = true;
                out.push(c);
            }
            ']' if class => {
                class = false;
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn fancy(pattern: &str) -> Fancy {
    // Python's re has no backtracking limit; neither does this.
    fancy_regex::RegexBuilder::new(&py_re(pattern)).backtrack_limit(usize::MAX).build().expect("a valid pattern")
}

fn plain(pattern: &str) -> Regex {
    Regex::new(&py_re(pattern)).expect("a valid pattern")
}

/// A name being defined: def/class/fn/func (with an optional Go receiver)/struct/... NAME, or UPPER_CASE = ...
pub static DEFINES: LazyLock<Fancy> = LazyLock::new(|| {
    fancy(&format!(
        r"\b(?:def|class|function|fn|func|struct|enum|trait|interface|type|module|record|object|protocol|const|static)\s+(?:\([^)]*\)\s*)?({IDENT})"
    ))
});
pub static CONSTANT: LazyLock<Fancy> = LazyLock::new(|| fancy(r"^\s*(?:export\s+)?([A-Z][A-Z0-9_]{2,})\s*(?::[^=]*)?=(?!=)"));
/// A C-family signature line: type tokens, then NAME(, ending in '{' (a definition, not a call statement).
/// Python's `$` (no MULTILINE) also matches before a final '\n', which `\s*` consumes here.
pub static SIGNATURE: LazyLock<Regex> =
    LazyLock::new(|| plain(&format!(r"^\s*(?:[\w<>\[\],*&:.]+\s+)+({IDENT})\s*\([^;]*\{{\s*\z")));
static HUNK: LazyLock<Regex> = LazyLock::new(|| plain(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@ ?(.*)"));
/// Names git prints unquoted.
static PLAIN: LazyLock<Regex> = LazyLock::new(|| plain(r#"^[^"\\\s\x00-\x1f\x7f]+\z"#));
static TEST_NAME: LazyLock<Regex> = LazyLock::new(|| plain(r"(_test|_spec|\.test|\.spec|Tests?)\.\w+\n?\z"));

fn found_all(re: &Fancy, text: &str) -> Vec<String> {
    re.captures_iter(text)
        .map(|c| c.expect("no backtracking limit").get(1).map_or(String::new(), |m| m.as_str().to_string()))
        .collect()
}

fn found_any(re: &Fancy, text: &str) -> bool {
    re.is_match(text).expect("no backtracking limit")
}

fn ctx(msg: impl Into<String>) -> Error {
    Error::new(Exc::Context, msg)
}

/// Python's `int()` of a `\d+` match: Unicode decimal digits count too. Line numbers past u64 saturate; no file
/// has that many lines, so no output changes.
fn digits(s: &str) -> u64 {
    s.chars().fold(0u64, |acc, c| acc.saturating_mul(10).saturating_add(py::decimal_digit(c).unwrap_or(0) as u64))
}

fn decode(data: Vec<u8>) -> String {
    match String::from_utf8(data) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

/// Run one read-only git command in repo with lazy fetching off; ContextError with git's message. `ok`: the exit
/// codes that count as success (a code in it still fails when git wrote to stderr).
pub fn git_binary_ok(repo: &Path, args: &[&str], ok: &[i32]) -> Result<Vec<u8>> {
    #[cfg(not(unix))]
    {
        let _ = (repo, args, ok);
        Err(Error::os(crate::rt::NATIVE_ONLY))
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let p = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .envs(ENV)
            .stdin(Stdio::inherit())
            .output()
            .map_err(|e| Error::io(&e, Some("git")))?;
        let code = p.status.code().unwrap_or_else(|| -p.status.signal().unwrap_or(0));
        // bytes.strip(): ASCII whitespace including \x0b, which trim_ascii keeps
        if !ok.contains(&code) || (code != 0 && p.stderr.iter().any(|c| !b" \t\n\r\x0b\x0c".contains(c))) {
            let text = decode(p.stderr);
            return Err(ctx(format!("git {} failed: {}", args[0], py::head(py::strip(&text), 500))));
        }
        Ok(p.stdout)
    }
}

/// `git(repo, *args)`: the output as text (invalid UTF-8 replaced).
pub fn git(repo: &Path, args: &[&str]) -> Result<String> {
    git_binary_ok(repo, args, &[0]).map(decode)
}

/// `git(repo, *args, binary=True)`.
pub fn git_binary(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_binary_ok(repo, args, &[0])
}

/// `git(repo, *args, ok=ok)`.
pub fn git_ok(repo: &Path, args: &[&str], ok: &[i32]) -> Result<String> {
    git_binary_ok(repo, args, ok).map(decode)
}

/// Full commit id of rev; a leading '-' is refused so a revision can never become a git option.
pub fn commit(repo: &Path, rev: &str, what: &str) -> Result<String> {
    if rev.is_empty() || rev.starts_with('-') {
        return Err(ctx(format!("{what} must be a commit, not {}", py::repr(rev))));
    }
    match git(repo, &["rev-parse", "--verify", "--quiet", "--end-of-options", &format!("{rev}^{{commit}}")]) {
        Ok(out) => Ok(py::strip(&out).to_string()),
        Err(e) if e.is(Exc::Context) => {
            Err(ctx(format!("{what} {} is not a commit in {}", py::repr(rev), repo.to_string_lossy())))
        }
        Err(e) => Err(e),
    }
}

/// Refuse a partial clone that lacks objects of these commits' trees: with lazy fetching off, missing blobs would
/// otherwise show up as silently absent context. rev-list prints every object; missing ones start with '?'.
pub fn complete(repo: &Path, revs: &[&str]) -> Result<()> {
    let mut args = vec!["rev-list", "--objects", "--missing=print", "--no-walk"];
    args.extend(revs);
    let out = git(repo, &args)?;
    let missing: Vec<&str> = py::splitlines(&out, false).into_iter().filter_map(|l| l.strip_prefix('?')).collect();
    if let Some(first) = missing.first() {
        return Err(ctx(format!(
            "the repository is an incomplete partial clone: {} object(s) of the base or head tree are missing \
             (first {first}). Fetch them first, for example `git fetch --refetch --no-filter origin <head>`; \
             Ringo never fetches.",
            missing.len()
        )));
    }
    Ok(())
}

/// (mode, type, object id) of path's tree entry at rev, or None if there is none. A submodule is a gitlink (mode
/// 160000, type commit) whose commit is not in this repository, so it has no text to read.
pub fn tree_entry(repo: &Path, rev: &str, path: &str) -> Result<Option<Vec<String>>> {
    let out = git(repo, &["ls-tree", "-z", rev, "--", path])?;
    let first = out.split('\0').next().unwrap_or("");
    if first.is_empty() {
        return Ok(None);
    }
    Ok(Some(first.split_once('\t').map_or(first, |(a, _)| a).split(' ').map(str::to_string).collect()))
}

/// (type, bytes) of an object.
pub type Object = (String, Vec<u8>);

struct Batch {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Every object read of one build through a single `git cat-file --batch`, each read once. A path containing a
/// newline cannot go through the line-based protocol, so it is read on its own.
pub struct Objects {
    repo: PathBuf,
    cache: HashMap<(String, String), Option<Object>>,
    proc: Option<Batch>,
}

impl Drop for Objects {
    fn drop(&mut self) {
        if let Some(Batch { mut child, stdin, stdout }) = self.proc.take() {
            drop(stdin);
            drop(stdout);
            let _ = child.wait();
        }
    }
}

impl Objects {
    pub fn new(repo: &Path) -> Objects {
        Objects { repo: repo.to_path_buf(), cache: HashMap::new(), proc: None }
    }

    /// (type, bytes) of rev:path, or None when it does not resolve to an object in this repository.
    pub fn read(&mut self, rev: &str, path: &str) -> Result<Option<Object>> {
        let key = (rev.to_string(), path.to_string());
        if let Some(found) = self.cache.get(&key) {
            return Ok(found.clone());
        }
        let found = if path.contains('\n') { self.single(rev, path)? } else { self.batch(rev, path)? };
        self.cache.insert(key, found.clone());
        Ok(found)
    }

    /// Python's `_single`: one `git cat-file blob` of its own.
    pub fn single(&self, rev: &str, path: &str) -> Result<Option<Object>> {
        match git_binary(&self.repo, &["cat-file", "blob", &format!("{rev}:{path}")]) {
            Ok(data) => Ok(Some(("blob".to_string(), data))),
            Err(e) if e.is(Exc::Context) => self.not_a_blob(rev, path),
            Err(e) => Err(e),
        }
    }

    fn not_a_blob(&self, rev: &str, path: &str) -> Result<Option<Object>> {
        Ok(tree_entry(&self.repo, rev, path)?.filter(|e| e[1] != "blob").map(|e| (e[1].clone(), Vec::new())))
    }

    fn batch(&mut self, rev: &str, path: &str) -> Result<Option<Object>> {
        #[cfg(not(unix))]
        {
            let _ = (rev, path);
            Err(Error::os(crate::rt::NATIVE_ONLY))
        }
        #[cfg(unix)]
        {
            if self.proc.is_none() {
                let mut child = Command::new("git")
                    .arg("-C")
                    .arg(&self.repo)
                    .args(["cat-file", "--batch"])
                    .envs(ENV)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| Error::io(&e, Some("git")))?;
                let stdin = child.stdin.take().expect("piped");
                let stdout = BufReader::new(child.stdout.take().expect("piped"));
                self.proc = Some(Batch { child, stdin, stdout });
            }
            let b = self.proc.as_mut().expect("started");
            b.stdin.write_all(format!("{rev}:{path}\n").as_bytes())?;
            b.stdin.flush()?;
            let mut header = Vec::new();
            b.stdout.read_until(b'\n', &mut header)?;
            if header.last() != Some(&b'\n') {
                return Err(ctx(format!("git cat-file --batch stopped while reading {rev}:{path}")));
            }
            let fields: Vec<&[u8]> = header[..header.len() - 1].split(|c| *c == b' ').collect();
            let last = fields.last().copied().unwrap_or(b"");
            if last == b"missing" || last == b"ambiguous" || last == b"submodule" {
                // a gitlink's commit is not in this repository either; newer git answers 'submodule'
                return self.not_a_blob(rev, path);
            }
            let (Some(kind), Some(size)) = (fields.get(1), fields.get(2)) else {
                return Err(Error::new(Exc::Index, "list index out of range"));
            };
            let kind = String::from_utf8_lossy(kind).into_owned();
            let Some(size) = std::str::from_utf8(size).ok().and_then(|s| s.parse::<usize>().ok()) else {
                return Err(Error::value(format!("invalid literal for int() with base 10: b'{}'", size.escape_ascii())));
            };
            let mut data = Vec::with_capacity(size);
            (&mut b.stdout).take(size as u64).read_to_end(&mut data)?;
            let mut end = [0u8; 1];
            let got = b.stdout.read(&mut end)?;
            if data.len() != size || got != 1 || end[0] != b'\n' {
                // never accept a cut-off object
                return Err(ctx(format!("git cat-file --batch returned a truncated object for {rev}:{path}")));
            }
            Ok(Some((kind, data)))
        }
    }
}

/// A file's text at rev, or None if it is binary, not UTF-8, or not a file (a submodule or a folder). objects: the
/// build's Objects reader; without one, the file is read by a git process of its own.
pub fn blob(repo: &Path, rev: &str, path: &str, objects: Option<&mut Objects>) -> Result<Option<String>> {
    let found = match objects {
        Some(o) => o.read(rev, path)?,
        None => Objects::new(repo).single(rev, path)?,
    };
    let Some((kind, data)) = found else {
        return Err(ctx(format!("git cat-file failed: {rev}:{path} does not exist")));
    };
    if kind != "blob" || data.contains(&0) {
        return Ok(None);
    }
    Ok(String::from_utf8(data).ok())
}

/// One change: (old path or None, new path or None).
pub type Pair = (Option<String>, Option<String>);

/// [(old_path or None, new_path or None)] for the three-dot change, in git's order, renames paired.
pub fn changes(repo: &Path, mb: &str, head: &str) -> Result<Vec<Pair>> {
    let out = git(repo, &["diff", "--no-ext-diff", "--no-textconv", "-M", "--name-status", "-z", mb, head])?;
    let fields: Vec<&str> = out.split('\0').collect();
    let at = |i: usize| fields.get(i).copied().unwrap_or("").to_string();
    let mut pairs = Vec::new();
    let mut i = 0;
    while i < fields.len() && !fields[i].is_empty() {
        let status = fields[i].chars().next();
        if matches!(status, Some('R' | 'C')) {
            pairs.push((if status == Some('R') { Some(at(i + 1)) } else { None }, Some(at(i + 2))));
            i += 3;
        } else {
            let path = at(i + 1);
            pairs.push((
                if status == Some('A') { None } else { Some(path.clone()) },
                if status == Some('D') { None } else { Some(path) },
            ));
            i += 2;
        }
    }
    Ok(pairs)
}

/// (old changed line numbers, new changed line numbers, hunk header contexts, removed lines, added lines).
pub type Hunk = (BTreeSet<u64>, BTreeSet<u64>, Vec<String>, Vec<String>, Vec<String>);

/// hunks() of one pair. text: this pair's section of the whole -U0 diff, when it was read once for every pair.
pub fn hunks(repo: &Path, mb: &str, head: &str, old: Option<&str>, new: Option<&str>, text: Option<&str>) -> Result<Hunk> {
    let read;
    let text = match text {
        Some(t) => t,
        None => {
            let mut args = vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv", "-M", "-U0", mb, head, "--"];
            for p in [old, new].into_iter().flatten() {
                if !p.is_empty() && !args[9..].contains(&p) {
                    args.push(p);
                }
            }
            read = git(repo, &args)?;
            &read
        }
    };
    let (mut olds, mut news, mut headers, mut minus, mut plus) = (BTreeSet::new(), BTreeSet::new(), vec![], vec![], vec![]);
    let mut started = false;
    for line in py::splitlines(text, false) {
        if let Some(m) = HUNK.captures(line) {
            started = true;
            let n = |i: usize, default: u64| m.get(i).map_or(default, |x| digits(x.as_str()));
            let (a, b, c, d) = (n(1, 0), n(2, 1), n(3, 0), n(4, 1));
            olds.extend(a..a.saturating_add(b));
            news.extend(c..c.saturating_add(d));
            headers.push(m.get(5).map_or("", |x| x.as_str()).to_string());
        } else if started && line.starts_with('-') {
            minus.push(line[1..].to_string());
        } else if started && line.starts_with('+') {
            plus.push(line[1..].to_string());
        }
    }
    Ok((olds, news, headers, minus, plus))
}

/// hunks() of every pair from one -U0 diff. git prints one 'diff --git' section per entry of changes(), in the same
/// order; if the sections do not line up with the pairs (count, or a plain path missing from its header), every
/// pair is read on its own.
pub fn all_hunks(repo: &Path, mb: &str, head: &str, pairs: &[Pair]) -> Result<Vec<Hunk>> {
    let text = git(repo, &["diff", "--no-color", "--no-ext-diff", "--no-textconv", "-M", "-U0", mb, head])?;
    // re.split(r'^(?=diff --git )', text, flags=re.M)[1:]
    let starts: Vec<usize> = (0..text.len())
        .filter(|&i| (i == 0 || text.as_bytes()[i - 1] == b'\n') && text.as_bytes()[i..].starts_with(b"diff --git "))
        .collect();
    let sections: Vec<&str> =
        starts.iter().enumerate().map(|(k, &a)| &text[a..starts.get(k + 1).copied().unwrap_or(text.len())]).collect();
    let lined_up = sections.len() == pairs.len()
        && pairs.iter().zip(&sections).all(|((old, new), section)| {
            let path = new.as_deref().filter(|p| !p.is_empty()).or(old.as_deref()).unwrap_or("");
            !PLAIN.is_match(path) || section.split('\n').next().unwrap_or("").contains(&format!(" b/{path}"))
        });
    let mut out = Vec::new();
    for (i, (old, new)) in pairs.iter().enumerate() {
        let section = if lined_up { Some(sections[i]) } else { None };
        out.push(hunks(repo, mb, head, old.as_deref(), new.as_deref(), section)?);
    }
    Ok(out)
}

/// (names, owners) of one source, or None if it does not parse.
pub type Symbols = Option<(BTreeSet<String>, BTreeSet<String>)>;

/// context.py's python_symbols, verbatim.
macro_rules! python_symbols_source {
    () => {
        r#"def python_symbols(source, lines):
    """(names, owners): names of the innermost def/class around each changed line and UPPER_CASE constants assigned
    on one, a dunder method standing for its class; owners are the classes of changed methods, searched for
    subclasses only, since a subclass inherits a changed method without naming it. None if the source does not parse."""
    try:
        tree = ast.parse(source)
    except (SyntaxError, ValueError):
        return None
    defs = (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
    found, owners = set(), set()

    def span(node):
        start = min([node.lineno, *(d.lineno for d in getattr(node, 'decorator_list', []))])
        return set(range(start, (node.end_lineno or node.lineno) + 1))

    def walk(node, owner):
        for child in ast.iter_child_nodes(node):
            if isinstance(child, defs):
                own = lines & span(child)
                if not own:
                    continue
                for inner in ast.walk(child):
                    if inner is not child and isinstance(inner, defs):
                        own -= span(inner)
                if own:  # some changed line belongs to child itself, not to a def nested in it
                    dunder = child.name.startswith('__') and child.name.endswith('__')
                    found.add(owner.name if dunder and isinstance(owner, ast.ClassDef) else child.name)
                    if isinstance(owner, ast.ClassDef):
                        owners.add(owner.name)
                walk(child, child)
                continue
            if isinstance(child, (ast.Assign, ast.AnnAssign)) and lines & set(range(child.lineno, child.end_lineno + 1)):
                targets = child.targets if isinstance(child, ast.Assign) else [child.target]
                found.update(t.id for t in targets if isinstance(t, ast.Name) and re.fullmatch(r'[A-Z][A-Z0-9_]{2,}', t.id))
            walk(child, owner)

    walk(tree, tree)
    return found, owners
"#
    };
}

/// The Python function the subprocess runs (the gate checks it against context.py's).
pub const PYTHON_SYMBOLS: &str = python_symbols_source!();

/// One request per source; the results stop at the first exception, which ends the list as {'raised': [name, str]}.
/// The recursion limit is lowered by the 6 frames `python3 ringo.py review` has above python_symbols beyond these
/// 3, so a deeply nested source raises RecursionError exactly where the Python runtime does.
const SCRIPT: &str = concat!(
    "import ast, json, re, sys\nsys.setrecursionlimit(sys.getrecursionlimit() - 6)\n\n\n",
    python_symbols_source!(),
    r#"

def main():
    out = []
    for source, lines in json.loads(sys.stdin.buffer.read()):
        try:
            found = python_symbols(source, set(lines))
        except BaseException as exc:
            out.append({'raised': [type(exc).__name__, str(exc)]})
            break
        out.append(None if found is None else [sorted(found[0]), sorted(found[1])])
    sys.stdout.write(json.dumps(out))


main()
"#
);

/// python_symbols of every (source, lines) in one `python3` process; Err is the first exception CPython raised
/// (RecursionError, MemoryError, ...), its class named as Python names it.
pub fn python_symbols_many(requests: &[(&str, &BTreeSet<u64>)]) -> Result<Vec<Symbols>> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    #[cfg(not(unix))]
    {
        Err(Error::os(crate::rt::NATIVE_ONLY))
    }
    #[cfg(unix)]
    {
        let input = serde_json::Value::Array(
            requests.iter().map(|(source, lines)| serde_json::json!([source, lines.iter().collect::<Vec<_>>()])).collect(),
        );
        let mut child = Command::new("python3")
            .args(["-P", "-c", SCRIPT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // ast.parse's SyntaxWarnings reach stderr, as in the Python runtime
            .spawn()
            .map_err(|e| Error::io(&e, Some("python3")))?;
        let mut stdin = child.stdin.take().expect("piped");
        let written = stdin.write_all(input.to_string().as_bytes());
        drop(stdin);
        let mut out = Vec::new();
        let read = child.stdout.take().expect("piped").read_to_end(&mut out);
        let status = child.wait()?;
        written?;
        read?;
        let parsed = match std::str::from_utf8(&out).ok().filter(|_| status.success()).map(crate::rt::pyjson::loads) {
            Some(Ok(serde_json::Value::Array(items))) => Some(items),
            _ => None,
        };
        let Some(parsed) = parsed else {
            return Err(Error::os(format!("python3 could not run python_symbols ({status})")));
        };
        let set = |v: &serde_json::Value| -> BTreeSet<String> {
            v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
        };
        let mut results = Vec::new();
        for item in parsed {
            if let Some(raised) = item.get("raised") {
                let name = raised[0].as_str().unwrap_or("Exception");
                let msg = raised[1].as_str().unwrap_or("").to_string();
                return Err(Error::new(Exc::from_name(name), msg));
            }
            results.push(if item.is_null() { None } else { Some((set(&item[0]), set(&item[1]))) });
        }
        Ok(results)
    }
}

/// (names, owners): names of the innermost def/class around each changed line and UPPER_CASE constants assigned on
/// one, a dunder method standing for its class; owners are the classes of changed methods. None if the source does
/// not parse. CPython's `ast`, through `python3`.
pub fn python_symbols(source: &str, lines: &BTreeSet<u64>) -> Result<Symbols> {
    Ok(python_symbols_many(&[(source, lines)])?.pop().flatten())
}

/// Names on definition-like changed lines and in hunk headers, for any language.
pub fn text_symbols(headers: &[String], changed: &[String]) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in headers.iter().chain(changed) {
        found.extend(found_all(&DEFINES, line));
        found.extend(found_all(&CONSTANT, line));
        if let Some(m) = SIGNATURE.captures(line)
            && !KEYWORDS.contains(&py::split(line)[0])
        {
            found.insert(m[1].to_string());
        }
    }
    found
}

/// PurePosixPath(path).parts for a relative path, as git prints them.
fn parts(path: &str) -> Vec<&str> {
    path.split('/').filter(|p| !p.is_empty() && *p != ".").collect()
}

/// PurePosixPath(path).suffix (Python 3.14: leading dots are not a suffix, a trailing one is).
fn suffix(path: &str) -> &str {
    let name = parts(path).last().copied().unwrap_or("");
    let name = name.trim_start_matches('.');
    name.rfind('.').map_or("", |i| &name[i..])
}

/// (changed symbol names, classes searched for subclasses only), each sorted, with common words and names under
/// three characters left out.
pub fn symbols(repo: &Path, mb: &str, head: &str, pairs: &[Pair], mut objects: Option<&mut Objects>) -> Result<(Vec<String>, Vec<String>)> {
    let all = all_hunks(repo, mb, head, pairs)?;
    let mut found = BTreeSet::new();
    let mut owners = BTreeSet::new();
    // Python sources go to one python3 process; a blob error stops the loop where Python's would stop, after
    // the python_symbols calls before it (whose own exception would have come first).
    // (source, changed lines, hunk headers, changed lines' text)
    type Source<'a> = (String, &'a BTreeSet<u64>, &'a [String], &'a [String]);
    let mut sources: Vec<Source> = Vec::new();
    let mut failed = None;
    'pairs: for ((old, new), (olds, news, headers, minus, plus)) in pairs.iter().zip(&all) {
        for (path, rev, lines, changed) in [(old, mb, olds, minus), (new, head, news, plus)] {
            let Some(path) = path.as_deref().filter(|p| !p.is_empty()) else { continue };
            if lines.is_empty() || PROSE.contains(&suffix(path).to_lowercase().as_str()) {
                continue;
            }
            if path.ends_with(".py") {
                match blob(repo, rev, path, objects.as_deref_mut()) {
                    Ok(text) => sources.push((text.unwrap_or_default(), lines, headers, changed)),
                    Err(e) => {
                        failed = Some(e);
                        break 'pairs;
                    }
                }
            } else {
                found.extend(text_symbols(headers, changed));
            }
        }
    }
    let requests: Vec<(&str, &BTreeSet<u64>)> = sources.iter().map(|(s, l, _, _)| (s.as_str(), *l)).collect();
    for (names, (_, _, headers, changed)) in python_symbols_many(&requests)?.into_iter().zip(&sources) {
        match names {
            None => found.extend(text_symbols(headers, changed)),
            Some((names, classes)) => {
                found.extend(names);
                owners.extend(classes);
            }
        }
    }
    if let Some(e) = failed {
        return Err(e);
    }
    let keep = |s: &&String| py::len(s) >= 3 && !COMMON.contains(s.as_str()) && !s.bytes().all(|c| c == b'_');
    let syms = found.iter().filter(keep).cloned().collect();
    let owners = owners.difference(&found).filter(keep).cloned().collect();
    Ok((syms, owners))
}

pub fn is_test(path: &str) -> bool {
    let parts = parts(path);
    let name = parts.last().copied().unwrap_or("");
    parts[..parts.len().saturating_sub(1)]
        .iter()
        .any(|p| ["test", "tests", "spec", "specs", "__tests__", "testing"].contains(p))
        || name.starts_with("test_")
        || TEST_NAME.is_match(name)
}

struct KindRes([Fancy; 6]);

impl KindRes {
    fn new(sym: &str) -> KindRes {
        let s = fancy_regex::escape(sym);
        KindRes([
            fancy(&format!(
                r"\b(?:def|class|function|fn|func|struct|enum|trait|interface|type|const|static|protocol)\s+(?:\([^)]*\)\s*)?{s}\b"
            )),
            fancy(&format!(r"^\s*(?:export\s+)?{s}\s*(?::[^=]*)?=(?!=)")),
            fancy(&format!(r"\bclass\s+{IDENT}\s*(?:\[[^\]]*\])?\s*\([^)]*\b{s}\b")),
            fancy(&format!(r"\b(?:extends|implements)\b[^{{]*\b{s}\b")),
            fancy(&format!(r"\b(?:class|struct|object)\s+{IDENT}[^{{(]*:\s*[^{{]*\b{s}\b")),
            fancy(&format!(r"\bimpl\b(?:<[^>]*>)?\s+(?:[\w:]+::)?{s}\b[^{{]*\bfor\b")),
        ])
    }

    fn kind(&self, line: &str) -> &'static str {
        let r = &self.0;
        if found_any(&r[0], line) || found_any(&r[1], line) {
            "definition"
        } else if r[2..].iter().any(|re| found_any(re, line)) {
            "subclass"
        } else {
            "reference"
        }
    }
}

thread_local! {
    static KINDS: std::cell::RefCell<HashMap<String, std::rc::Rc<KindRes>>> = std::cell::RefCell::new(HashMap::new());
}

/// How a matching line relates to sym: it defines it, subclasses/implements it, or refers to it.
pub fn kind(line: &str, sym: &str) -> &'static str {
    let res = KINDS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= 512 && !cache.contains_key(sym) {
            cache.clear(); // compiled patterns are cached as Python's re caches them, within a bound
        }
        cache.entry(sym.to_string()).or_insert_with(|| std::rc::Rc::new(KindRes::new(sym))).clone()
    });
    res.kind(line)
}

/// `git grep` lines with a word match of any name at head: one PCRE2 alternation, or, for a git without PCRE or a
/// name outside [A-Za-z0-9_], one -F -w pattern per name, whose word rule is the same.
pub fn grep(repo: &Path, head: &str, names: &[String]) -> Result<String> {
    let common = ["grep", "-n", "-I", "-z", "--no-color", "--no-column"];
    if names.iter().all(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')) {
        let pattern = format!("(?<![A-Za-z0-9_])(?:{})(?![A-Za-z0-9_])", names.join("|"));
        let mut args = common.to_vec();
        args.extend(["-P", "-e", &pattern, head, "--"]);
        match git_ok(repo, &args, &[0, 1]) {
            Ok(out) => return Ok(out),
            Err(e) if e.is(Exc::Context) => {} // git built without PCRE, or a pattern too large for it
            Err(e) => return Err(e),
        }
    }
    let mut args = common.to_vec();
    args.extend(["-w", "-F"]);
    for n in names {
        args.extend(["-e", n]);
    }
    args.extend([head, "--"]);
    git_ok(repo, &args, &[0, 1])
}

/// One candidate file: the matched line numbers and the reasons ((kind, sym)).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub lines: BTreeSet<u64>,
    pub why: BTreeSet<(&'static str, String)>,
}

/// {path: Entry} for word matches of syms at head outside exclude; an owner class counts only where a line
/// subclasses it.
pub fn matches(repo: &Path, head: &str, syms: &[String], owners: &[String], exclude: &BTreeSet<String>) -> Result<BTreeMap<String, Entry>> {
    let mut found: BTreeMap<String, Entry> = BTreeMap::new();
    if syms.is_empty() && owners.is_empty() {
        return Ok(found);
    }
    let names: Vec<String> = syms.iter().chain(owners).cloned().collect();
    let out = grep(repo, head, &names)?;
    let alternation: Vec<String> = names.iter().map(|n| fancy_regex::escape(n).into_owned()).collect();
    let word = fancy(&format!("(?<![A-Za-z0-9_])({})(?![A-Za-z0-9_])", alternation.join("|")));
    let only_subclass: BTreeSet<&str> = owners.iter().map(String::as_str).collect();
    let prefix = format!("{head}:");
    for rec in out.split('\n') {
        let bits: Vec<&str> = rec.split('\0').collect();
        if bits.len() < 3 || !bits[0].starts_with(&prefix) {
            continue;
        }
        let path = &bits[0][prefix.len()..];
        let lineno: u64 = py::strip(bits[1]).parse().map_err(|_| {
            Error::value(format!("invalid literal for int() with base 10: {}", py::repr(bits[1])))
        })?;
        let text = bits[2..].join("\0");
        if exclude.contains(path) {
            continue;
        }
        let test = is_test(path);
        let mut why = BTreeSet::new();
        for sym in found_all(&word, &text) {
            let k = kind(&text, &sym);
            if !only_subclass.contains(sym.as_str()) || k == "subclass" {
                why.insert((if test { "test" } else { k }, sym));
            }
        }
        if !why.is_empty() {
            let entry = found.entry(path.to_string()).or_default();
            entry.lines.insert(lineno);
            entry.why.extend(why);
        }
    }
    Ok(found)
}

/// A non-negative integer of any size, little-endian 64-bit limbs: rank's exact Fraction sums.
#[derive(Clone, PartialEq, Eq)]
struct Big(Vec<u64>);

impl Big {
    fn mul_small(&mut self, m: u64) {
        let mut carry = 0u128;
        for limb in &mut self.0 {
            let x = *limb as u128 * m as u128 + carry;
            *limb = x as u64;
            carry = x >> 64;
        }
        if carry > 0 {
            self.0.push(carry as u64);
        }
    }

    fn div_small(&self, d: u64) -> (Big, u64) {
        let mut q = vec![0u64; self.0.len()];
        let mut r = 0u128;
        for i in (0..self.0.len()).rev() {
            let x = (r << 64) | self.0[i] as u128;
            q[i] = (x / d as u128) as u64;
            r = x % d as u128;
        }
        while q.last() == Some(&0) {
            q.pop();
        }
        (Big(q), r as u64)
    }

    fn add(&mut self, o: &Big) {
        let mut carry = 0u128;
        for i in 0..self.0.len().max(o.0.len()) {
            if i == self.0.len() {
                self.0.push(0);
            }
            let x = self.0[i] as u128 + *o.0.get(i).unwrap_or(&0) as u128 + carry;
            self.0[i] = x as u64;
            carry = x >> 64;
        }
        if carry > 0 {
            self.0.push(carry as u64);
        }
    }
}

impl Ord for Big {
    fn cmp(&self, o: &Big) -> std::cmp::Ordering {
        self.0.len().cmp(&o.0.len()).then_with(|| self.0.iter().rev().cmp(o.0.iter().rev()))
    }
}

impl PartialOrd for Big {
    fn partial_cmp(&self, o: &Big) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Candidates in deterministic order: definitions, subclasses, references, then tests; inside each, files that touch
/// more and rarer changed symbols first (a match on a name found in n files weighs 1/n, summed exactly), then by path.
pub fn rank(found: BTreeMap<String, Entry>) -> Vec<(String, Entry)> {
    let syms = |e: &Entry| e.why.iter().map(|(_, s)| s.clone()).collect::<BTreeSet<String>>();
    let mut spread: HashMap<String, u64> = HashMap::new();
    for entry in found.values() {
        for sym in syms(entry) {
            *spread.entry(sym).or_default() += 1;
        }
    }
    // Every 1/n over the common denominator L = lcm of the spreads: L/n is exact.
    let mut l = Big(vec![1]);
    for &n in spread.values().collect::<BTreeSet<_>>() {
        let g = gcd(l.div_small(n).1, n);
        l.mul_small(n / g);
    }
    let share: HashMap<u64, Big> = spread.values().map(|&n| (n, l.div_small(n).0)).collect();
    let mut keyed: Vec<(usize, Big, String, Entry)> = found
        .into_iter()
        .map(|(path, entry)| {
            let best = entry.why.iter().map(|(k, _)| KIND_ORDER.iter().position(|o| o == k).unwrap_or(0)).min().unwrap_or(0);
            let mut score = Big(Vec::new());
            for sym in syms(&entry) {
                score.add(&share[&spread[&sym]]);
            }
            (best, score, path, entry)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)).then_with(|| a.2.cmp(&b.2)));
    keyed.into_iter().map(|(_, _, path, entry)| (path, entry)).collect()
}

/// Line-numbered windows around the matched lines, merged, as many as fit in room bytes counting each window's
/// 'a-b, ' in the manifest: (text, shown spans, total spans). Lines split on '\n' only, as git grep -n counts them.
pub fn windows(text: &str, lines: &BTreeSet<u64>, room: i64) -> (String, Vec<String>, usize) {
    let mut rows: Vec<&str> = text.split('\n').map(|r| r.strip_suffix('\r').unwrap_or(r)).collect();
    if rows.last() == Some(&"") {
        rows.pop();
    }
    let count = rows.len() as i64;
    let mut spans: Vec<(i64, i64)> = Vec::new();
    for &n in lines {
        let n = n.min(i64::MAX as u64 / 2) as i64;
        let (a, b) = (1.max(n - WINDOW), count.min(n + WINDOW));
        match spans.last_mut() {
            Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
            _ => spans.push((a, b)),
        }
    }
    let mut out: Vec<String> = Vec::new();
    let mut used = 0i64;
    for &(a, b) in &spans {
        let mut part = format!("--- lines {a}-{b} ---\n");
        for i in a..=b {
            part.push_str(&format!("{i}: {}\n", rows[(i - 1) as usize]));
        }
        let cost = (part.len() + format!("{a}-{b}, ").len()) as i64;
        if used + cost > room {
            break;
        }
        out.push(part);
        used += cost;
    }
    let shown = spans[..out.len()].iter().map(|(a, b)| format!("{a}-{b}")).collect();
    (out.concat(), shown, spans.len())
}

pub fn describe(why: &BTreeSet<(&'static str, String)>) -> String {
    let order = |k: &str| KIND_ORDER.iter().position(|o| *o == k).unwrap_or(0);
    let word = |k: &str| match k {
        "definition" => "defines",
        "subclass" => "subclasses or implements",
        "reference" => "refers to",
        _ => "tests or uses",
    };
    let mut items: Vec<&(&str, String)> = why.iter().collect();
    items.sort_by(|a, b| order(a.0).cmp(&order(b.0)).then_with(|| a.1.cmp(&b.1)));
    items.iter().map(|(k, s)| format!("{} {s}", word(k))).collect::<Vec<_>>().join("; ")
}

pub fn listing(items: &[String]) -> Vec<String> {
    let mut shown: Vec<String> = items.iter().take(LISTED).map(|x| format!("- {x}")).collect();
    if items.len() > LISTED {
        shown.push(format!("- ... and {} more", items.len() - LISTED));
    }
    if shown.is_empty() {
        shown.push("- none".to_string());
    }
    shown
}

/// The review material for base...head in repo, as one string of at most `budget` UTF-8 bytes unless the change
/// itself (PR text, diff, changed files, README) is already larger; then it has no context blocks.
pub fn build(repo: &Path, base: &str, head: &str, pr_text: Option<&str>, budget: i64) -> Result<String> {
    let repo = py::path(repo);
    if budget <= 0 {
        return Err(ctx("the context budget must be a positive number of bytes"));
    }
    if !repo.is_dir() {
        return Err(ctx(format!("{} is not a folder", repo.to_string_lossy())));
    }
    let mut objects = Objects::new(&repo);
    _build(&repo, base, head, pr_text, budget, &mut objects)
}

struct Added {
    block: String,
    note: String,
    skip: String,
}

fn _build(repo: &Path, base: &str, head: &str, pr_text: Option<&str>, budget: i64, objects: &mut Objects) -> Result<String> {
    let base_id = commit(repo, base, "base")?;
    let head_id = commit(repo, head, "head")?;
    let mb = py::strip(&git(repo, &["merge-base", &base_id, &head_id])?).to_string();
    let mut revs: Vec<&str> = Vec::new();
    for r in [head_id.as_str(), mb.as_str(), base_id.as_str()] {
        if !revs.contains(&r) {
            revs.push(r);
        }
    }
    complete(repo, &revs)?;
    let diff = git(
        repo,
        &["diff", "--no-color", "--no-ext-diff", "--no-textconv", "-M", "--src-prefix=a/", "--dst-prefix=b/", &format!("{base_id}...{head_id}")],
    )?;
    let pairs = changes(repo, &mb, &head_id)?;
    let changed: Vec<String> = pairs.iter().filter_map(|(_, new)| new.clone().filter(|n| !n.is_empty())).collect();
    let mut parts = Vec::new();
    if let Some(pr) = pr_text {
        parts.push(format!("=== PR description ===\n{pr}"));
    }
    parts.push(format!("=== change.diff ===\n{diff}"));
    let mut submodules = Vec::new();
    for path in &changed {
        let text = match blob(repo, &head_id, path, Some(objects))? {
            Some(text) => text,
            None => match tree_entry(repo, &head_id, path)? {
                Some(e) if e[1] == "commit" => {
                    submodules.push(format!("{path} at {}", e[2]));
                    format!("(submodule at commit {}; its contents are not in this repository and are not shown)\n", e[2])
                }
                _ => "(binary or not UTF-8 text; not shown)\n".to_string(),
            },
        };
        parts.push(format!("=== {path} (after the change) ===\n{text}"));
    }
    let mut readme = None;
    for r in READMES {
        if !changed.iter().any(|c| c == r) && objects.read(&head_id, r)?.is_some_and(|(kind, _)| kind == "blob") {
            readme = Some(r);
            break;
        }
    }
    if let Some(r) = readme
        && let Some(text) = blob(repo, &head_id, r, Some(objects))?
    {
        parts.push(format!("=== {r} ===\n{text}"));
    }
    let required = parts.join("\n");
    let (syms, owners) = symbols(repo, &mb, &head_id, &pairs, Some(objects))?;
    let mut exclude: BTreeSet<String> = changed.iter().cloned().collect();
    exclude.extend(readme.map(str::to_string));
    let found = matches(repo, &head_id, &syms, &owners, &exclude)?;

    let names = |list: &[String]| {
        let mut s = list.iter().take(LISTED).cloned().collect::<Vec<_>>().join(", ");
        if s.is_empty() {
            s = "none".to_string();
        }
        if list.len() > LISTED {
            s.push_str(&format!(", ... and {} more", list.len() - LISTED));
        }
        s
    };
    let always = [pr_text.map(|_| "the PR description".to_string()), Some("the three-dot diff".to_string()),
                  Some(format!("{} changed file(s) at head", changed.len())), readme.map(str::to_string)];
    let always = always.into_iter().flatten().collect::<Vec<_>>().join(", ");
    let manifest = |added: &[Added], skipped: &[String], unreadable: &[String]| -> String {
        let mut lines = vec![
            "=== context manifest ===".to_string(),
            format!("Built by Ringo from git objects, read-only: base {base_id}, head {head_id}, merge-base {mb}; budget {budget} bytes."),
            format!("Always included: {always}."),
            format!("Changed symbols searched with word matches at head ({}): {}.", syms.len(), names(&syms)),
            format!("Classes of changed methods, searched for subclasses only ({}): {}.", owners.len(), names(&owners)),
            "Added as unchanged context, in order:".to_string(),
        ];
        lines.extend(listing(&added.iter().map(|a| a.note.clone()).collect::<Vec<_>>()));
        lines.push("Left out for the byte budget:".to_string());
        lines.extend(listing(skipped));
        lines.push("Left out as binary or not UTF-8:".to_string());
        lines.extend(listing(unreadable));
        if !submodules.is_empty() {
            lines.push("Changed submodules, shown only as their commit:".to_string());
            lines.extend(listing(&submodules));
        }
        lines.push("Everything else in the repository was not included; a claim that rests on it is still a question.".to_string());
        lines.join("\n") + "\n"
    };
    // bytes a manifest line adds; lines past LISTED are summarized, so they cost nothing
    let note = |items: &mut Vec<String>, line: String| -> i64 {
        let cost = format!("- {line}\n").len() as i64;
        items.push(line);
        if items.len() <= LISTED { cost } else { 0 }
    };
    let (mut added, mut skipped, mut unreadable): (Vec<Added>, Vec<String>, Vec<String>) = (vec![], vec![], vec![]);
    // Bytes so far: the change, the empty manifest, and slack for the '... and N more' lines. Exact check at the end.
    let mut used = required.len() as i64 + 1 + manifest(&added, &skipped, &unreadable).len() as i64 + 3 * 40;
    for (path, entry) in rank(found) {
        let why = describe(&entry.why);
        let label = format!("=== {path} (unchanged, at head) ===\n");
        let room = budget - used - 1 - label.len() as i64 - format!("- {path}: lines ; {why}\n").len() as i64
            - " (partial: 000 of 000 windows)".len() as i64;
        if room <= 0 {
            used += note(&mut skipped, format!("{path}: {why}"));
            continue;
        }
        let text = match blob(repo, &head_id, &path, Some(objects)) {
            Ok(text) => text,
            Err(e) if e.is(Exc::Context) => None,
            Err(e) => return Err(e),
        };
        let Some(text) = text else {
            used += note(&mut unreadable, path);
            continue;
        };
        let (block, line) = if (text.len() as i64) <= WHOLE_FILE.min(room) {
            (label + &text, format!("{path}: whole file; {why}"))
        } else {
            let (shown, spans, count) = windows(&text, &entry.lines, room);
            if shown.is_empty() {
                used += note(&mut skipped, format!("{path}: {why}"));
                continue;
            }
            let partial = if spans.len() == count { String::new() } else { format!(" (partial: {} of {count} windows)", spans.len()) };
            (label + &shown, format!("{path}: lines {}{partial}; {why}", spans.join(", ")))
        };
        let cost = 1 + block.len() as i64;
        added.push(Added { block, note: line.clone(), skip: format!("{path}: {why}") });
        used += cost + if added.len() <= LISTED { format!("- {line}\n").len() as i64 } else { 0 };
    }
    let material = |added: &[Added], skipped: &[String], unreadable: &[String]| -> String {
        let mut all = vec![required.as_str()];
        all.extend(added.iter().map(|a| a.block.as_str()));
        let m = manifest(added, skipped, unreadable);
        all.push(&m);
        all.join("\n")
    };
    while !added.is_empty() && material(&added, &skipped, &unreadable).len() as i64 > budget {
        // only if the estimate above was short
        let last = added.pop().expect("not empty");
        skipped.insert(0, last.skip);
    }
    if skipped.len() > 1 && (required.len() as i64) < budget && budget < material(&added, &skipped, &unreadable).len() as i64 {
        // the list itself overflows
        skipped = vec![format!("{} unchanged file(s) that reference the change", skipped.len())];
    }
    Ok(material(&added, &skipped, &unreadable))
}
