"""Review material from a git repository: the change plus the unchanged code it depends on.

A review call is isolated and single-turn; on a tool-free backend it can only judge what the material shows. With
only the diff and the changed files, a defect that rests on an unchanged caller, base class, subclass or test turns into an
open question or is missed. This module builds that context in code, before any call: deterministic (the same
commits and budget give the same bytes), read-only (git object reads only; the work tree and index are never
touched) and offline (GIT_NO_LAZY_FETCH=1, so a partial clone never fetches; an incomplete one is refused).

Material, in order: the PR description, the three-dot diff, every changed file at head, the README at head, then
'=== <path> (unchanged, at head) ===' blocks for definitions, subclasses/implementations and references of the
symbols the change touches (tests last), then a '=== context manifest ===' saying what was added, why, and what
the budget left out. The budget never drops the change itself: it only limits the context blocks. A changed
submodule (gitlink) has no text here, so it shows as its new commit and is listed in the manifest.

Symbols: for Python, the innermost function, method or class around each changed line on both sides (the old side
matters: callers of a removed or renamed function break), plus changed UPPER_CASE constants; for other languages,
names on definition-like changed lines and in hunk headers. References come from word matches at head (git grep), so they are
word matches on names, not resolved bindings: common names can pull in unrelated files, which ranking demotes.
"""
import ast
import os
import re
import subprocess
from fractions import Fraction
from pathlib import Path, PurePosixPath

BUDGET = 400_000      # bytes of material, including the change itself and the manifest
WHOLE_FILE = 16_000   # an unchanged file up to this size goes in whole; a larger one as windows around matches
WINDOW = 15           # lines of context above and below each match in a windowed file
LISTED = 200          # manifest lines per list before it summarizes the rest
CHANGED_WINDOW = 25   # lines above and below each changed line in a windowed changed file (to be tuned by measurement)
REFILL_STEPS = (60, 150)  # auto mode re-widens windowed changed files through these, then to the whole file
LONG_LINE = 1000      # in windowed mode a longer line is cut to this many characters
OUTLINE_LISTED = 200  # outline lines of one windowed changed file before it summarizes the rest
MODES = ('auto', 'full', 'windowed')
# Lock files and generated files (windowed mode): no file block, no diff section, only a stat line.
LOCK_NAMES = frozenset({'Cargo.lock', 'package-lock.json', 'npm-shrinkwrap.json', 'yarn.lock', 'pnpm-lock.yaml',
                        'poetry.lock', 'uv.lock', 'Pipfile.lock', 'composer.lock', 'Gemfile.lock', 'go.sum', 'flake.lock'})
GENERATED_SUFFIXES = ('.min.js', '.min.css', '.map')
GENERATED_MARKS = ('@generated', 'DO NOT EDIT')  # in a file's first five lines
READMES = ('README.md', 'README.rst', 'README.txt', 'README')
PROSE = {'.md', '.markdown', '.rst', '.txt', '.adoc'}  # changed prose names no symbols ('the function above...')
# GIT_LITERAL_PATHSPECS: a changed path such as 'pages/[id].tsx' or ':x' names that file, never a glob or magic.
ENV = {'GIT_NO_LAZY_FETCH': '1', 'GIT_TERMINAL_PROMPT': '0', 'GIT_OPTIONAL_LOCKS': '0', 'GIT_LITERAL_PATHSPECS': '1'}

IDENT = r'[A-Za-z_][A-Za-z0-9_]*'
# A name being defined: def/class/fn/func (with an optional Go receiver)/struct/... NAME, or UPPER_CASE = ...
DEFINES = re.compile(r'\b(?:def|class|function|fn|func|struct|enum|trait|interface|type|module|record|object|'
                     rf'protocol|const|static)\s+(?:\([^)]*\)\s*)?({IDENT})')
CONSTANT = re.compile(r'^\s*(?:export\s+)?([A-Z][A-Z0-9_]{2,})\s*(?::[^=]*)?=(?!=)')
# A C-family signature line: type tokens, then NAME(, ending in '{' (a definition, not a call statement).
SIGNATURE = re.compile(rf'^\s*(?:[\w<>\[\],*&:.]+\s+)+({IDENT})\s*\([^;]*\{{\s*$')
KEYWORDS = {'return', 'new', 'else', 'throw', 'await', 'yield', 'case', 'delete', 'typeof', 'if', 'while', 'for',
            'switch', 'catch', 'do', 'goto', 'sizeof', 'not', 'and', 'or', 'in', 'is', 'elif', 'except', 'with'}
COMMON = KEYWORDS | {'self', 'cls', 'this', 'super', 'main', 'init', 'test', 'setup', 'run', 'get', 'set', 'None',
                     'True', 'False', 'null', 'nil', 'true', 'false', 'object', 'string', 'value', 'data', 'name',
                     'type', 'args', 'kwargs', 'result', 'error', 'list', 'dict', 'str', 'int', 'bool', 'void',
                     'const', 'static', 'public', 'private', 'protected', 'def', 'class', 'function', 'struct',
                     'impl', 'pub', 'async', 'var', 'let', 'val', 'export', 'default', 'import', 'from'}
KIND_ORDER = ('definition', 'subclass', 'reference', 'test')
HUNK = re.compile(r'@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@ ?(.*)')
PLAIN_NAME = re.compile(r'[^"\\\s\x00-\x1f\x7f]+')  # names git prints unquoted
PY_DEF = re.compile(r'^\s*(?:async\s+)?(?:def|class)\b')  # a Python definition line, for the outline


class ContextError(ValueError):
    """The repository or revisions cannot give complete, offline material."""


def git(repo, *args, binary=False, ok=(0,), input=None):
    """Run one read-only git command in repo with lazy fetching off (input, bytes, on its stdin); raise ContextError
    with git's message."""
    p = subprocess.run(['git', '-C', str(repo), *args], capture_output=True, env={**os.environ, **ENV}, input=input)
    if p.returncode not in ok or (p.returncode and p.stderr.strip()):
        raise ContextError(f"git {args[0]} failed: {p.stderr.decode('utf-8', 'replace').strip()[:500]}")
    return p.stdout if binary else p.stdout.decode('utf-8', 'replace')


def commit(repo, rev, what):
    """Full commit id of rev; a leading '-' is refused so a revision can never become a git option."""
    if not rev or rev.startswith('-'):
        raise ContextError(f'{what} must be a commit, not {rev!r}')
    try:
        return git(repo, 'rev-parse', '--verify', '--quiet', '--end-of-options', f'{rev}^{{commit}}').strip()
    except ContextError:
        raise ContextError(f'{what} {rev!r} is not a commit in {repo}') from None


def complete(repo, *revs):
    """Refuse a partial clone that lacks objects of these commits' trees: with lazy fetching off, missing blobs
    would otherwise show up as silently absent context. rev-list prints every object; missing ones start with '?'."""
    missing = [line[1:] for line in git(repo, 'rev-list', '--objects', '--missing=print', '--no-walk', *revs).splitlines()
               if line.startswith('?')]
    if missing:
        raise ContextError(f'the repository is an incomplete partial clone: {len(missing)} object(s) of the base or '
                           f'head tree are missing (first {missing[0]}). Fetch them first, for example '
                           '`git fetch --refetch --no-filter origin <head>`; Ringo never fetches.')


def tree_entry(repo, rev, path):
    """(mode, type, object id) of path's tree entry at rev, or None if there is none. A submodule is a gitlink
    (mode 160000, type commit) whose commit is not in this repository, so it has no text to read."""
    out = git(repo, 'ls-tree', '-z', rev, '--', path).split('\0')[0]
    return tuple(out.split('\t', 1)[0].split(' ')) if out else None


class Objects:
    """Every object read of one build through a single `git cat-file --batch`, each read once: a git process per
    file cost about 25 ms, which dominated large changes. A path containing a newline cannot go through the
    line-based protocol, so it is read on its own."""

    def __init__(self, repo):
        self.repo, self.cache, self.proc = repo, {}, None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        if self.proc:
            self.proc.stdin.close()
            self.proc.stdout.close()
            self.proc.wait()

    def read(self, rev, path):
        """(type, bytes) of rev:path, or None when it does not resolve to an object in this repository."""
        key = (rev, path)
        if key not in self.cache:
            self.cache[key] = self._single(rev, path) if '\n' in path else self._batch(rev, path)
        return self.cache[key]

    def _single(self, rev, path):
        try:
            return 'blob', git(self.repo, 'cat-file', 'blob', f'{rev}:{path}', binary=True)
        except ContextError:
            e = tree_entry(self.repo, rev, path)
            return (e[1], b'') if e and e[1] != 'blob' else None

    def _batch(self, rev, path):
        if self.proc is None:
            self.proc = subprocess.Popen(['git', '-C', str(self.repo), 'cat-file', '--batch'], stdin=subprocess.PIPE,
                                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env={**os.environ, **ENV})
        self.proc.stdin.write(f'{rev}:{path}\n'.encode('utf-8', 'surrogateescape'))
        self.proc.stdin.flush()
        header = self.proc.stdout.readline()
        if not header.endswith(b'\n'):
            raise ContextError(f'git cat-file --batch stopped while reading {rev}:{path}')
        fields = header[:-1].split(b' ')
        # missing/ambiguous: a gitlink's commit is not in this repository either; newer git answers 'submodule'
        if fields[-1] in (b'missing', b'ambiguous', b'submodule'):
            e = tree_entry(self.repo, rev, path)
            return (e[1], b'') if e and e[1] != 'blob' else None
        kind, size = fields[1].decode(), int(fields[2])
        data = self.proc.stdout.read(size)
        if len(data) != size or self.proc.stdout.read(1) != b'\n':  # never accept a cut-off object
            raise ContextError(f'git cat-file --batch returned a truncated object for {rev}:{path}')
        return kind, data


def blob(repo, rev, path, objects=None):
    """A file's text at rev, or None if it is binary, not UTF-8, or not a file (a submodule or a folder).
    objects: the build's Objects reader; without one, the file is read by a git process of its own."""
    found = objects.read(rev, path) if objects else Objects(repo)._single(rev, path)
    if found is None:
        raise ContextError(f'git cat-file failed: {rev}:{path} does not exist')
    kind, data = found
    if kind != 'blob':
        return None
    if b'\0' in data:
        return None
    try:
        return data.decode('utf-8')
    except UnicodeDecodeError:
        return None


def changes(repo, mb, head):
    """[(old_path or None, new_path or None)] for the three-dot change, in git's order, renames paired."""
    fields = git(repo, 'diff', '--no-ext-diff', '--no-textconv', '-M', '--name-status', '-z', mb, head).split('\0')
    out, i = [], 0
    while i < len(fields) and fields[i]:
        status = fields[i][0]
        if status in 'RC':
            out.append((fields[i + 1] if status == 'R' else None, fields[i + 2]))
            i += 3
        else:
            path = fields[i + 1]
            out.append((None if status == 'A' else path, None if status == 'D' else path))
            i += 2
    return out


def hunks(repo, mb, head, old, new, text=None):
    """(old changed line numbers, new changed line numbers, hunk header contexts, removed lines, added lines).
    text: this pair's section of the whole -U0 diff, when it was read once for every pair (see all_hunks)."""
    if text is None:
        text = pair_diff(repo, mb, head, old, new)
    olds, news, headers, minus, plus = set(), set(), [], [], []
    started = False
    for line in text.splitlines():
        m = HUNK.match(line)
        if m:
            started = True
            a, b, c, d = int(m[1]), int(m[2] or 1), int(m[3]), int(m[4] or 1)
            olds.update(range(a, a + b))
            news.update(range(c, c + d))
            headers.append(m[5])
        elif started and line[:1] == '-':
            minus.append(line[1:])
        elif started and line[:1] == '+':
            plus.append(line[1:])
    return olds, news, headers, minus, plus


def pair_diff(repo, mb, head, old, new):
    """The -U0 diff of one changed pair, read on its own."""
    paths = [p for p in dict.fromkeys((old, new)) if p]
    return git(repo, 'diff', '--no-color', '--no-ext-diff', '--no-textconv', '-M', '-U0', mb, head, '--', *paths)


def aligned_sections(text, pairs):
    """One 'diff --git' section of a whole diff per pair, or None if they do not line up with the pairs (count, or a
    plain path missing from its header). git prints one section per entry of changes(), in the same order."""
    sections = re.split(r'^(?=diff --git )', text, flags=re.M)[1:]
    if len(sections) == len(pairs) and all(
            not PLAIN_NAME.fullmatch(path or '') or f' b/{path}' in section.split('\n', 1)[0]
            for (old, new), section in zip(pairs, sections) for path in [new or old]):
        return sections
    return None


def diff_texts(repo, mb, head, pairs):
    """The -U0 diff text of every pair from one git process instead of one per changed file; if the sections do not
    line up with the pairs, every pair is read on its own."""
    sections = aligned_sections(git(repo, 'diff', '--no-color', '--no-ext-diff', '--no-textconv', '-M', '-U0', mb, head), pairs)
    return sections if sections is not None else [pair_diff(repo, mb, head, old, new) for old, new in pairs]


def all_hunks(repo, mb, head, pairs):
    """hunks() of every pair (see diff_texts)."""
    return [hunks(repo, mb, head, old, new, text=text) for (old, new), text in zip(pairs, diff_texts(repo, mb, head, pairs))]


def new_points(text):
    """The new-side lines a -U0 diff changes, one line for a pure deletion: the line that now sits where the deleted
    lines were (hunks() has none for it, its new-side range is empty). Callers clamp them to the file."""
    points = set()
    for line in text.splitlines():
        m = HUNK.match(line)
        if m:
            c, d = int(m[3]), int(m[4] or 1)
            points.update(range(c, c + d) if d else [c + 1])
    return points


def python_symbols(source, lines):
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


def text_symbols(headers, changed):
    """Names on definition-like changed lines and in hunk headers, for any language."""
    found = set()
    for line in [*headers, *changed]:
        found.update(DEFINES.findall(line))
        found.update(CONSTANT.findall(line))
        m = SIGNATURE.match(line)
        if m and line.split()[0] not in KEYWORDS:
            found.add(m[1])
    return found


def symbols(repo, mb, head, pairs, objects=None):
    """(changed symbol names, classes searched for subclasses only), each sorted, with common words and names
    under three characters left out."""
    found, owners = set(), set()
    for (old, new), (olds, news, headers, minus, plus) in zip(pairs, all_hunks(repo, mb, head, pairs)):
        sides = [(old, mb, olds, minus), (new, head, news, plus)]
        for path, rev, lines, changed in sides:
            if not path or not lines or PurePosixPath(path).suffix.lower() in PROSE:
                continue
            names = python_symbols(blob(repo, rev, path, objects) or '', lines) if path.endswith('.py') else None
            if names is None:
                found |= text_symbols(headers, changed)
            else:
                found |= names[0]
                owners |= names[1]
    keep = lambda s: len(s) >= 3 and s not in COMMON and not re.fullmatch(r'_+', s)
    return sorted(filter(keep, found)), sorted(filter(keep, owners - found))


def is_test(path):
    parts = PurePosixPath(path).parts
    name = parts[-1]
    return (any(p in ('test', 'tests', 'spec', 'specs', '__tests__', 'testing') for p in parts[:-1])
            or re.match(r'test_', name) is not None
            or re.search(r'(_test|_spec|\.test|\.spec|Tests?)\.\w+$', name) is not None)


def kind(line, sym):
    """How a matching line relates to sym: it defines it, subclasses/implements it, or refers to it."""
    s = re.escape(sym)
    if re.search(rf'\b(?:def|class|function|fn|func|struct|enum|trait|interface|type|const|static|protocol)\s+'
                 rf'(?:\([^)]*\)\s*)?{s}\b', line) or re.match(rf'\s*(?:export\s+)?{s}\s*(?::[^=]*)?=(?!=)', line):
        return 'definition'
    if (re.search(rf'\bclass\s+{IDENT}\s*(?:\[[^\]]*\])?\s*\([^)]*\b{s}\b', line)
            or re.search(rf'\b(?:extends|implements)\b[^{{]*\b{s}\b', line)
            or re.search(rf'\b(?:class|struct|object)\s+{IDENT}[^{{(]*:\s*[^{{]*\b{s}\b', line)
            or re.search(rf'\bimpl\b(?:<[^>]*>)?\s+(?:[\w:]+::)?{s}\b[^{{]*\bfor\b', line)):
        return 'subclass'
    return 'reference'


GREP_CHUNK = 16_000  # bytes of names per PCRE pattern


def grep(repo, head, names):
    """`git grep` lines with a word match of any name at head. One PCRE2 alternation is a single JIT-compiled pass;
    one -F -w pattern per name made git scan each line once per name (7.3 s against 0.27 s for 54 names over a
    large repository, with the same output). A git without PCRE, or a name outside [A-Za-z0-9_], uses the -F -w
    patterns, whose word rule is the same."""
    common = ['grep', '-n', '-I', '-z', '--no-color', '--no-column']
    if all(re.fullmatch(r'[A-Za-z0-9_]+', n) for n in names):
        # git's PCRE refuses one alternation past about 31 KB: several patterns, one per stdin line (no command-line
        # limit, which is 32,767 characters on Windows), print the same lines once each
        groups, size = [[]], 0
        for n in names:
            if groups[-1] and size + len(n) + 1 > GREP_CHUNK:
                groups.append([])
                size = 0
            groups[-1].append(n)
            size += len(n) + 1
        patterns = '\n'.join(rf'(?<![A-Za-z0-9_])(?:{"|".join(g)})(?![A-Za-z0-9_])' for g in groups)
        try:
            return git(repo, *common, '-P', '-f', '-', head, '--', ok=(0, 1), input=patterns.encode())  # 1: no match
        except ContextError:
            pass  # git built without PCRE, or a pattern too large for it
    return git(repo, *common, '-w', '-F', '-f', '-', head, '--', ok=(0, 1), input='\n'.join(names).encode())


def matches(repo, head, syms, owners, exclude):
    """{path: {'lines': {lineno}, 'why': {(kind, sym)}}} for word matches of syms at head outside exclude; an owner
    class counts only where a line subclasses it."""
    if not syms and not owners:
        return {}
    out = grep(repo, head, [*syms, *owners])
    word = re.compile(r'(?<![A-Za-z0-9_])(' + '|'.join(map(re.escape, [*syms, *owners])) + r')(?![A-Za-z0-9_])')
    only_subclass = set(owners)
    found = {}
    prefix = f'{head}:'
    for rec in out.split('\n'):
        bits = rec.split('\0')
        if len(bits) < 3 or not bits[0].startswith(prefix):
            continue
        path, lineno, text = bits[0][len(prefix):], int(bits[1]), '\0'.join(bits[2:])
        if path in exclude:
            continue
        why = set()
        for sym in dict.fromkeys(word.findall(text)):
            k = kind(text, sym)
            if sym not in only_subclass or k == 'subclass':
                why.add(('test' if is_test(path) else k, sym))
        if why:
            entry = found.setdefault(path, {'lines': set(), 'why': set()})
            entry['lines'].add(lineno)
            entry['why'] |= why
    return found


def rank(found):
    """Candidates in deterministic order: definitions, subclasses, references, then tests; inside each, files that
    touch more and rarer changed symbols first (a match on a name found in n files weighs 1/n), then by path."""
    spread = {}
    for entry in found.values():
        for sym in {s for _, s in entry['why']}:
            spread[sym] = spread.get(sym, 0) + 1
    def key(item):
        path, entry = item
        best = min(KIND_ORDER.index(k) for k, _ in entry['why'])
        score = sum((Fraction(1, spread[s]) for s in {s for _, s in entry['why']}), Fraction(0))
        return best, -score, path
    return sorted(found.items(), key=key)


def line_rows(text):
    """A file's lines: split on '\\n' only, as git grep -n counts them (str.splitlines would also split on form feeds
    and other separators and shift every window), without a trailing '\\r' or a final empty line."""
    rows = [row.removesuffix('\r') for row in text.split('\n')]
    if rows[-1] == '':
        rows.pop()
    return rows


def spans_around(count, lines, window):
    """[a, b] line spans of `window` lines above and below each line (of count lines), merged when they touch."""
    spans = []
    for n in sorted(lines):
        a, b = max(1, n - window), min(count, n + window)
        if spans and a <= spans[-1][1] + 1:
            spans[-1][1] = max(spans[-1][1], b)
        else:
            spans.append([a, b])
    return spans


def span_text(rows, a, b):
    return f'--- lines {a}-{b} ---\n' + ''.join(f'{i}: {rows[i - 1]}\n' for i in range(a, b + 1))


def windows(text, lines, room):
    """Line-numbered windows around the matched lines, merged, as many as fit in room bytes counting each window's
    'a-b, ' in the manifest: (text, shown spans, total spans)."""
    rows = line_rows(text)
    spans = spans_around(len(rows), lines, WINDOW)
    out, used = [], 0
    for a, b in spans:
        part = span_text(rows, a, b)
        cost = len(part.encode()) + len(f'{a}-{b}, ')
        if used + cost > room:
            break
        out.append(part)
        used += cost
    return ''.join(out), [f'{a}-{b}' for a, b in spans[:len(out)]], len(spans)


def cut_line(row):
    """A line over LONG_LINE characters, cut (minified code, data): it would fill the budget with one line."""
    if len(row) <= LONG_LINE:
        return row
    return f'{row[:LONG_LINE]} \u2026[{len(row) - LONG_LINE} more characters cut]'


def cut_long_lines(text):
    return '\n'.join(cut_line(row) for row in text.split('\n'))


def has_mark(text):
    """The text says in its first five lines that it is generated."""
    return any(mark in line for line in text.split('\n')[:5] for mark in GENERATED_MARKS)


def generated_kind(path, text, base_text, deleted):
    """How a changed text file counts as generated, or None: 'lock' for a lock file or a minified or source-map file
    (by name: the file block and the diff section become a stat line), 'marked' for a file whose first five lines say
    it is generated (the file block becomes a stat line; the diff stays, since the marker is text the change itself can
    add). text: the file at head, or at the merge base for a deletion; base_text: the file at the merge base, if it
    existed there as text. A marker the change adds to an existing file does not classify it."""
    if text is None:
        return None
    name = PurePosixPath(path).name
    if name in LOCK_NAMES or name.endswith(GENERATED_SUFFIXES):
        return 'lock'
    if not deleted and has_mark(text) and (base_text is None or has_mark(base_text)):
        return 'marked'
    return None


def outline(rows, spans, python):
    """The definition-like lines outside the spans, numbered, as the text after '--- outline ---'; text_symbols' own
    detection (DEFINES, CONSTANT, SIGNATURE), plus def and class lines for Python."""
    inside = set()
    for a, b in spans:
        inside.update(range(a, b + 1))
    found = [i for i, row in enumerate(rows, 1)
             if i not in inside and (text_symbols([], [row]) or (python and PY_DEF.match(row)))]
    lines = ['--- outline ---', *(f'{i}: {rows[i - 1]}' for i in found[:OUTLINE_LISTED])]
    if len(found) > OUTLINE_LISTED:
        lines.append(f'... and {len(found) - OUTLINE_LISTED} more')
    return '\n'.join(lines) + '\n'


def changed_windows(path, rows, points, width):
    """(block text after the label, shown spans 'a-b') of a large changed file: windows of +-width lines around each
    changed line and the outline of the definitions outside them. rows are the file's lines, long ones already cut."""
    count = len(rows)
    spans = spans_around(count, {min(max(n, 1), count) for n in points}, width)
    head = (f'(windowed: {count} lines at head; shown: \u00b1{width} lines around each change and the outline below; '
            'other lines are unchanged since the merge base)\n')
    return (head + ''.join(span_text(rows, a, b) for a, b in spans) + outline(rows, spans, path.endswith('.py')),
            [f'{a}-{b}' for a, b in spans])


def describe(why):
    order = {k: i for i, k in enumerate(KIND_ORDER)}
    words = {'definition': 'defines', 'subclass': 'subclasses or implements', 'reference': 'refers to',
             'test': 'tests or uses'}
    return '; '.join(f'{words[k]} {s}' for k, s in sorted(why, key=lambda w: (order[w[0]], w[1])))


def listing(items):
    shown = [f'- {x}' for x in items[:LISTED]]
    if len(items) > LISTED:
        shown.append(f'- ... and {len(items) - LISTED} more')
    return shown or ['- none']


def build(repo, base, head, pr_text=None, budget=BUDGET, mode='auto', changed_window=CHANGED_WINDOW):
    """The review material for base...head in repo, as one string of at most `budget` UTF-8 bytes unless the change
    itself (PR text, diff, changed files, README) is already larger; then it has no context blocks.

    mode: 'full' sends every changed file whole (the control of any comparison); 'windowed' sends a changed file
    over WHOLE_FILE as windows of +-changed_window lines around its changes plus an outline, and lock and generated
    files as a stat line; 'auto' builds 'full' and, only if the change alone is over the budget, 'windowed' instead,
    then spends what the budget has left re-widening the windows. If windowed does not fit either, auto returns
    the smaller of the two builds."""
    repo = Path(repo)
    if not isinstance(budget, int) or budget <= 0:
        raise ContextError('the context budget must be a positive number of bytes')
    if mode not in MODES:
        raise ContextError(f'the context mode must be auto, full or windowed, not {mode!r}')
    if not isinstance(changed_window, int) or changed_window < 0:
        raise ContextError('the changed window must be a non-negative number of lines')
    if not repo.is_dir():
        raise ContextError(f'{repo} is not a folder')
    with Objects(repo) as objects:
        if mode == 'auto':
            material, need = _build(repo, base, head, pr_text, budget, objects, False, changed_window)
            if need <= budget:
                return material
            windowed = _build(repo, base, head, pr_text, budget, objects, True, changed_window, auto_need=need)[0]
            # Windows are meant to make the change fit. If they still do not, send whichever is smaller (a tie: full).
            if len(windowed.encode()) <= budget or len(windowed.encode()) < len(material.encode()):
                return windowed
            return material
        return _build(repo, base, head, pr_text, budget, objects, mode == 'windowed', changed_window)[0]


def replace_generated(diff, pairs, stats):
    """(diff, matched): the diff with the section of each lock file cut to its 'diff --git' line and a stat line, and
    the set of change indices that were. stats is {change index: (added, deleted)}. A section belongs to a change by
    its header, 'diff --git a/OLD b/NEW' with plain names; a change is cut only if exactly one section is its own and
    no other change has the same header (a type change prints two sections; an unusual name cannot be matched), so
    anything else stays in the diff."""
    first = re.split(r'^(?=diff --git )', diff, flags=re.M)
    sections = first[1:]
    expected = {}
    for i, (old, new) in enumerate(pairs):
        a, b = old or new, new or old
        if PLAIN_NAME.fullmatch(a) and PLAIN_NAME.fullmatch(b):
            expected.setdefault(f'diff --git a/{a} b/{b}', []).append(i)
    hits = {}
    for k, section in enumerate(sections):
        owners = expected.get(section.split('\n', 1)[0], [])
        if len(owners) == 1:
            hits.setdefault(owners[0], []).append(k)
    matched = {i for i in stats if len(hits.get(i, [])) == 1}
    cut = {hits[i][0]: stats[i] for i in matched}
    return first[0] + ''.join(
        section.split('\n', 1)[0] + f'\n(generated or lock file: +{cut[k][0]} -{cut[k][1]} lines; not shown)\n'
        if k in cut else section for k, section in enumerate(sections)), matched


def generated_kind_of(repo, mb, head, old, new, objects):
    """generated_kind of one change: a deletion by its merge-base text, anything else by its text at head and, if the
    file existed, at the merge base."""
    try:
        base_text = blob(repo, mb, old, objects) if old else None
    except ContextError:
        base_text = None
    return generated_kind(new or old, blob(repo, head, new, objects) if new else base_text, base_text, new is None)


def _build(repo, base, head, pr_text, budget, objects, windowed=False, window=CHANGED_WINDOW, auto_need=None):
    """(material, bytes of the change alone: PR text, diff, changed files, README). auto_need: set when auto mode
    chose windowed because the full change needed that many bytes; windowed changed files are then re-widened."""
    base_id, head_id = commit(repo, base, 'base'), commit(repo, head, 'head')
    mb = git(repo, 'merge-base', base_id, head_id).strip()
    complete(repo, *dict.fromkeys((head_id, mb, base_id)))
    diff = git(repo, 'diff', '--no-color', '--no-ext-diff', '--no-textconv', '-M', '--src-prefix=a/', '--dst-prefix=b/',
               f'{base_id}...{head_id}')
    pairs = changes(repo, mb, head_id)
    changed = [new for _, new in pairs if new]
    # windowed, by change index: (added, removed) line counts, new-side changed lines, and how it counts as generated
    stats, points, kinds = {}, {}, {}
    if windowed:
        for i, ((old, new), text) in enumerate(zip(pairs, diff_texts(repo, mb, head_id, pairs))):
            _, _, _, minus, plus = hunks(repo, mb, head_id, old, new, text=text)
            stats[i], points[i] = (len(plus), len(minus)), new_points(text)
            kinds[i] = generated_kind_of(repo, mb, head_id, old, new, objects)
    pair_of = {new: i for i, (_, new) in enumerate(pairs) if new}
    parts = []
    if pr_text is not None:
        parts.append(f'=== PR description ===\n{pr_text}')
    diff_at = len(parts)
    parts.append(f'=== change.diff ===\n{diff}')
    submodules, wfiles = [], []  # wfiles: windowed changed files, in change order
    for path in changed:
        text = blob(repo, head_id, path, objects)
        if text is None:
            e = tree_entry(repo, head_id, path)
            if e and e[1] == 'commit':
                submodules.append(f'{path} at {e[2]}')
                text = f'(submodule at commit {e[2]}; its contents are not in this repository and are not shown)\n'
            else:
                text = '(binary or not UTF-8 text; not shown)\n'
        elif windowed and kinds[pair_of[path]]:
            added, deleted = stats[pair_of[path]]
            parts.append(f'=== {path} (generated or lock file; not shown) ===\n+{added} -{deleted} lines\n')
            continue
        elif windowed and len(text.encode()) > WHOLE_FILE:
            entry = {'path': path, 'text': text, 'rows': [cut_line(r) for r in line_rows(text)],
                     'points': points[pair_of[path]], 'index': len(parts), 'whole': False}
            wide(entry, window)
            wfiles.append(entry)
            parts.append(entry['block'])
            continue
        parts.append(f'=== {path} (after the change) ===\n{text}')
    locks = {i: stats[i] for i, kind in kinds.items() if kind == 'lock'}
    matched = set()
    if locks:
        cut, matched = replace_generated(diff, pairs, locks)
        parts[diff_at] = f'=== change.diff ===\n{cut}'
    readme = next((r for r in READMES if r not in changed and (objects.read(head_id, r) or ('',))[0] == 'blob'), None)
    if readme:
        text = blob(repo, head_id, readme, objects)
        if text is not None:
            parts.append(f'=== {readme} ===\n{text}')
    required = '\n'.join(parts)
    change_bytes = len(required.encode())
    syms, owners = symbols(repo, mb, head_id, pairs, objects)
    found = matches(repo, head_id, syms, owners, set(changed) | ({readme} if readme else set()))

    added, skipped, unreadable = [], [], []
    cut_notes, kept_notes = [], []
    for i, kind in sorted(kinds.items()):
        name = pairs[i][1] or pairs[i][0]
        if kind == 'lock' and i in matched:
            cut_notes.append(f'{name}: +{stats[i][0]} -{stats[i][1]} lines')
        elif kind == 'lock':
            kept_notes.append(f'{name}: generated or lock file; diff kept (could not be matched)')
        elif kind == 'marked':
            kept_notes.append(f'{name}: +{stats[i][0]} -{stats[i][1]} lines; file block shown as a stat line (marked generated); diff kept')
    def manifest():
        wnotes = [f"{w['path']}: lines {', '.join(w['spans'])} of {len(w['rows'])}" if w['spans']
                  else f"{w['path']}: no changed lines of {len(w['rows'])}" for w in wfiles if not w['whole']]
        lines = ['=== context manifest ===',
                 f'Built by Ringo from git objects, read-only: base {base_id}, head {head_id}, merge-base {mb}; '
                 f'budget {budget} bytes' + ('; changed files: windowed' if windowed else '') + '.',
                 *([f'(auto mode: windowed because the change alone is {auto_need} bytes, over the {budget}-byte budget)']
                   if auto_need is not None else []),
                 'Always included: ' + ', '.join(filter(None, ['the PR description' if pr_text is not None else None,
                                                             'the three-dot diff', f'{len(changed)} changed file(s) at head',
                                                             readme])) + '.',
                 *(['Changed files sent as windows:', *listing(wnotes),
                    'Generated or lock files shown as a stat line (file block and diff):', *listing(cut_notes),
                    'Generated or lock files whose diff was kept:', *listing(kept_notes)] if windowed else []),
                 f'Changed symbols searched with word matches at head ({len(syms)}): ' + (', '.join(syms[:LISTED]) or 'none')
                 + (f', ... and {len(syms) - LISTED} more' if len(syms) > LISTED else '') + '.',
                 f'Classes of changed methods, searched for subclasses only ({len(owners)}): '
                 + (', '.join(owners[:LISTED]) or 'none') + (f', ... and {len(owners) - LISTED} more' if len(owners) > LISTED else '') + '.',
                 'Added as unchanged context, in order:', *listing([a['note'] for a in added]),
                 'Left out for the byte budget:', *listing(skipped),
                 'Left out as binary or not UTF-8:', *listing(unreadable),
                 *(['Changed submodules, shown only as their commit:', *listing(submodules)] if submodules else []),
                 'Everything else in the repository was not included; a claim that rests on it is still a question.']
        return '\n'.join(lines) + '\n'
    def note(items, line):  # bytes a manifest line adds; lines past LISTED are summarized, so they cost nothing
        items.append(line)
        return len(f'- {line}\n'.encode()) if len(items) <= LISTED else 0
    # Bytes so far: the change, the empty manifest, and slack for the '... and N more' lines. Exact check at the end.
    used = len(required.encode()) + 1 + len(manifest().encode()) + 3 * 40
    for path, entry in rank(found):
        why = describe(entry['why'])
        label = f'=== {path} (unchanged, at head) ===\n'
        room = budget - used - 1 - len(label.encode()) - len(f'- {path}: lines ; {why}\n'.encode()) - len(' (partial: 000 of 000 windows)')
        if room <= 0:
            used += note(skipped, f'{path}: {why}')
            continue
        try:
            text = blob(repo, head_id, path, objects)
        except ContextError:
            text = None
        if text is None:
            used += note(unreadable, path)
            continue
        if windowed:
            text = cut_long_lines(text)
        if len(text.encode()) <= min(WHOLE_FILE, room):
            block, line = label + text, f'{path}: whole file; {why}'
        else:
            shown, spans, count = windows(text, entry['lines'], room)
            if not shown:
                used += note(skipped, f'{path}: {why}')
                continue
            partial = '' if len(spans) == count else f' (partial: {len(spans)} of {count} windows)'
            block, line = label + shown, f'{path}: lines {", ".join(spans)}{partial}; {why}'
        added.append({'block': block, 'note': line, 'skip': f'{path}: {why}'})
        used += 1 + len(block.encode()) + (len(f'- {line}\n'.encode()) if len(added) <= LISTED else 0)
    def material():
        return '\n'.join([required, *[a['block'] for a in added], manifest()])
    while added and len(material().encode()) > budget:  # only if the estimate above was short
        skipped.insert(0, added.pop()['skip'])
    if len(skipped) > 1 and len(required.encode()) < budget < len(material().encode()):  # the list itself overflows
        skipped[:] = [f'{len(skipped)} unchanged file(s) that reference the change']
    if auto_need is not None:
        # Auto mode: the windows were the price of fitting; the context blocks are placed, so spend what is left on
        # wider windows. Steps in order: every file at 60 lines, then at 150, then whole; a file takes a step only if
        # the whole material still fits the budget.
        for step in [*(s for s in REFILL_STEPS if s > window), None]:
            for w in wfiles:
                if w['whole']:
                    continue
                before = {k: w[k] for k in ('block', 'spans', 'width', 'whole')}
                if step is None:
                    w.update(block=whole_block(w), whole=True)
                else:
                    wide(w, step)
                parts[w['index']], was = w['block'], required
                required = '\n'.join(parts)
                if len(material().encode()) > budget:
                    w.update(before)
                    parts[w['index']], required = before['block'], was
    result = material()
    excess = len(result.encode()) - budget
    if excess > 0:  # the change itself (PR text, diff, changed files, README) is over budget: context blocks were
        # dropped first; the manifest says by how much instead of sending oversized material silently.
        overflow = ('Over the budget by {} bytes before this notice: required material and its manifest do not fit. Additional context '
                    'may have been omitted. A claim that rests on what is not shown is still a question.'.format(excess))
        return '\n'.join([required, *[a['block'] for a in added], manifest() + overflow + '\n']), change_bytes
    return result, change_bytes


def whole_block(entry):
    return f"=== {entry['path']} (after the change) ===\n{entry['text']}"


def wide(entry, width):
    """Set a windowed changed file's block, spans and width: windows of +-width lines around its changes. A block
    that is not smaller in bytes than the whole file (a new file, or windows covering nearly all of it, with their
    line numbers and outline) is the whole file instead: windowing never sends more than full mode does."""
    body, entry['spans'] = changed_windows(entry['path'], entry['rows'], entry['points'], width)
    block, entry['width'] = f"=== {entry['path']} (after the change) ===\n{body}", width
    if len(block.encode()) >= len(whole_block(entry).encode()):
        block, entry['whole'] = whole_block(entry), True
    entry['block'] = block
