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


class ContextError(ValueError):
    """The repository or revisions cannot give complete, offline material."""


def git(repo, *args, binary=False, ok=(0,)):
    """Run one read-only git command in repo with lazy fetching off; raise ContextError with git's message."""
    p = subprocess.run(['git', '-C', str(repo), *args], capture_output=True, env={**os.environ, **ENV})
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
        if fields[-1] in (b'missing', b'ambiguous'):  # a gitlink's commit is not in this repository either
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
        paths = [p for p in dict.fromkeys((old, new)) if p]
        text = git(repo, 'diff', '--no-color', '--no-ext-diff', '--no-textconv', '-M', '-U0', mb, head, '--', *paths)
    olds, news, headers, minus, plus = set(), set(), [], [], []
    started = False
    for line in text.splitlines():
        m = re.match(r'@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@ ?(.*)', line)
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


def all_hunks(repo, mb, head, pairs):
    """hunks() of every pair from one -U0 diff instead of one git process per changed file. git prints one
    'diff --git' section per entry of changes(), in the same order; if the sections do not line up with the
    pairs (count, or a plain path missing from its header), every pair is read on its own as before."""
    text = git(repo, 'diff', '--no-color', '--no-ext-diff', '--no-textconv', '-M', '-U0', mb, head)
    sections = re.split(r'^(?=diff --git )', text, flags=re.M)[1:]
    plain = re.compile(r'[^"\\\s\x00-\x1f\x7f]+')  # names git prints unquoted
    if len(sections) == len(pairs) and all(
            not plain.fullmatch(path or '') or f' b/{path}' in section.split('\n', 1)[0]
            for (old, new), section in zip(pairs, sections) for path in [new or old]):
        return [hunks(repo, mb, head, old, new, text=section) for (old, new), section in zip(pairs, sections)]
    return [hunks(repo, mb, head, old, new) for old, new in pairs]


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


def grep(repo, head, names):
    """`git grep` lines with a word match of any name at head. One PCRE2 alternation is a single JIT-compiled pass;
    one -F -w pattern per name made git scan each line once per name (7.3 s against 0.27 s for 54 names over a
    large repository, with the same output). A git without PCRE, or a name outside [A-Za-z0-9_], uses the -F -w
    patterns, whose word rule is the same."""
    common = ['grep', '-n', '-I', '-z', '--no-color', '--no-column']
    if all(re.fullmatch(r'[A-Za-z0-9_]+', n) for n in names):
        pattern = rf'(?<![A-Za-z0-9_])(?:{"|".join(names)})(?![A-Za-z0-9_])'
        try:
            return git(repo, *common, '-P', '-e', pattern, head, '--', ok=(0, 1))  # 1: no match
        except ContextError:
            pass  # git built without PCRE, or a pattern too large for it
    return git(repo, *common, '-w', '-F', *[x for n in names for x in ('-e', n)], head, '--', ok=(0, 1))


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


def windows(text, lines, room):
    """Line-numbered windows around the matched lines, merged, as many as fit in room bytes counting each window's
    'a-b, ' in the manifest: (text, shown spans, total spans). Lines split on '\\n' only, as git grep -n counts
    them; str.splitlines would also split on form feeds and other separators and shift every window."""
    rows = [row.removesuffix('\r') for row in text.split('\n')]
    if rows[-1] == '':
        rows.pop()
    spans = []
    for n in sorted(lines):
        a, b = max(1, n - WINDOW), min(len(rows), n + WINDOW)
        if spans and a <= spans[-1][1] + 1:
            spans[-1][1] = max(spans[-1][1], b)
        else:
            spans.append([a, b])
    out, used = [], 0
    for a, b in spans:
        part = f'--- lines {a}-{b} ---\n' + ''.join(f'{i}: {rows[i - 1]}\n' for i in range(a, b + 1))
        cost = len(part.encode()) + len(f'{a}-{b}, ')
        if used + cost > room:
            break
        out.append(part)
        used += cost
    return ''.join(out), [f'{a}-{b}' for a, b in spans[:len(out)]], len(spans)


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


def build(repo, base, head, pr_text=None, budget=BUDGET):
    """The review material for base...head in repo, as one string of at most `budget` UTF-8 bytes unless the change
    itself (PR text, diff, changed files, README) is already larger; then it has no context blocks."""
    repo = Path(repo)
    if not isinstance(budget, int) or budget <= 0:
        raise ContextError('the context budget must be a positive number of bytes')
    if not repo.is_dir():
        raise ContextError(f'{repo} is not a folder')
    with Objects(repo) as objects:
        return _build(repo, base, head, pr_text, budget, objects)


def _build(repo, base, head, pr_text, budget, objects):
    base_id, head_id = commit(repo, base, 'base'), commit(repo, head, 'head')
    mb = git(repo, 'merge-base', base_id, head_id).strip()
    complete(repo, *dict.fromkeys((head_id, mb, base_id)))
    diff = git(repo, 'diff', '--no-color', '--no-ext-diff', '--no-textconv', '-M', '--src-prefix=a/', '--dst-prefix=b/',
               f'{base_id}...{head_id}')
    pairs = changes(repo, mb, head_id)
    changed = [new for _, new in pairs if new]
    parts = []
    if pr_text is not None:
        parts.append(f'=== PR description ===\n{pr_text}')
    parts.append(f'=== change.diff ===\n{diff}')
    submodules = []
    for path in changed:
        text = blob(repo, head_id, path, objects)
        if text is None:
            e = tree_entry(repo, head_id, path)
            if e and e[1] == 'commit':
                submodules.append(f'{path} at {e[2]}')
                text = f'(submodule at commit {e[2]}; its contents are not in this repository and are not shown)\n'
            else:
                text = '(binary or not UTF-8 text; not shown)\n'
        parts.append(f'=== {path} (after the change) ===\n{text}')
    readme = next((r for r in READMES if r not in changed and (objects.read(head_id, r) or ('',))[0] == 'blob'), None)
    if readme:
        text = blob(repo, head_id, readme, objects)
        if text is not None:
            parts.append(f'=== {readme} ===\n{text}')
    required = '\n'.join(parts)
    syms, owners = symbols(repo, mb, head_id, pairs, objects)
    found = matches(repo, head_id, syms, owners, set(changed) | ({readme} if readme else set()))

    added, skipped, unreadable = [], [], []
    def manifest():
        lines = ['=== context manifest ===',
                 f'Built by Ringo from git objects, read-only: base {base_id}, head {head_id}, merge-base {mb}; '
                 f'budget {budget} bytes.',
                 'Always included: ' + ', '.join(filter(None, ['the PR description' if pr_text is not None else None,
                                                             'the three-dot diff', f'{len(changed)} changed file(s) at head',
                                                             readme])) + '.',
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
    return material()
