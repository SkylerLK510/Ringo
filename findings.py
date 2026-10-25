"""Findings from a Ringo review, located in files, for scorers that match per file (ReviewBench).

parse() splits a review into its numbered `### N.` findings the way pipelines.findings() does (fenced code cannot
start one; the Open questions and Not checked sections end them) and locates each one: the `- Location:` line,
else the anchors in the heading's trailing parentheses (or its backticked spans), else the first anchor in the
body. A finding anchored to several files becomes one entry per file, because a per-file matcher can only match
it inside a file. A file named without a line gets line 1. Pure and offline.

anchor() checks the entries against the repository at head with read-only git object reads (context.git): a file
missing at head is dropped and counted, unless an `a/` or `b/` diff prefix or a unique path suffix (with base, a
unique changed one) names one file; lines are clamped to the file. reviewbench() builds the ReviewBench agent-contract JSON.
"""
import re

import context
import pipelines

MESSAGE = 2500  # characters per message
SEG = r'[\w.@+\-\[\]]'
# A path ('/' or '\\' separated, cleaned later): segments with a final '.ext' (lower-case led, or one capital such as .R), or at least one '/', or a
# well-known bare file name. Not inside a longer token, and not followed by more path characters.
FILE = (rf'(?<![\w./\\@+\-\[\]])(/?(?:{SEG}+[/\\])*{SEG}*\.(?:[a-z][A-Za-z0-9]{{0,9}}|[A-Z])|/?(?:{SEG}+[/\\])+{SEG}+'
        rf'|Dockerfile|Makefile|Gemfile|Rakefile|Procfile)(?![\w@+\-\[\]/]|\.\w)')
LOC = re.compile(FILE + r'`?(?:(?::L?|#L|:\s*lines?\s+)~?(\d+)(?:\s*[-–]\s*L?~?(\d+))?)?')
FIELD = r'^\s*[-*]\s*\**{}\**\s*:\s*\**\s*(.*?)\s*\**\s*$'
FIELD_START = re.compile(r'^(\s*)[-*]\s*\**[A-Z][\w -]{0,30}\**\s*:')
TRAILING = re.compile(r'\(([^()]*(?:\([^()]*\)[^()]*)*)\)[\s*.]*$')


def clean(path):
    """A repository-relative path: forward slashes, no leading './' or '/'."""
    path = path.replace('\\', '/')
    while path.startswith('./'):
        path = path[2:]
    return path.lstrip('/')


def locations(text, need_line=False):
    """[(file, start, end)] anchors in text, first one per file; a file without a line gets line 1. need_line keeps
    only anchors with a line number or a directory, which prose mentions such as `d.db` rarely have."""
    out, seen = [], set()
    for m in LOC.finditer(text):
        path = clean(m.group(1))
        if not path or path in seen or (need_line and not m.group(2) and '/' not in path):
            continue
        seen.add(path)
        start = max(1, int(m.group(2) or 1))
        out.append((path, start, max(start, int(m.group(3) or start))))
    return out


def heading_anchors(title):
    """Anchors in a heading: those in its trailing parentheses, else backticked spans in it that have a line or a
    directory (a bare `name.ext` in a title is as often code, such as `d.db`, as a file)."""
    m = TRAILING.search(title)
    found = locations(m.group(1)) if m else []
    return found or locations(' '.join(re.findall(r'`([^`\n]+)`', title)), need_line=True)


def field(body, name):
    m = re.search(FIELD.format(name), body, re.M | re.I)
    return m.group(1) if m and m.group(1) else None


def problem(lines):
    """The text of the `- Problem:` bullet with its wrapped and nested lines, or None."""
    for k, line in enumerate(lines):
        m = re.match(r'^(\s*)[-*]\s*\**Problem\**\s*:\s*\**\s*(.*)$', line, re.I)
        if not m:
            continue
        indent, out = len(m.group(1)), [m.group(2)]
        for nxt in lines[k + 1:]:
            if nxt.strip():
                depth = len(nxt) - len(nxt.lstrip())
                if depth <= indent and (FIELD_START.match(nxt) or nxt.lstrip().startswith('#')):
                    break
                if depth <= indent and not out[-1].strip():  # a paragraph after a blank line ends the bullet
                    break
            out.append(nxt)
        text = ' '.join(' '.join(out).split())
        return text or None
    return None


def parse(review_text):
    """[{file, start_line, end_line, message, finding, severity?, category?}] for a Ringo review; `finding` is the
    finding's number in the review, shared by the entries of a finding anchored to several files. A finding with
    no file anchor gives no entry."""
    out = []
    _, found, _ = pipelines.findings('f', review_text)
    for n, body in enumerate(found.values(), 1):
        head, _, rest = body.partition('\n')
        title = pipelines.HEAD.match(head).group(2).strip(' *')
        lines = rest.splitlines()
        loc = field(rest, 'Location')
        anchors = (locations(loc) if loc else []) or heading_anchors(title) \
            or (locations(rest, need_line=True) or locations(rest))[:1]
        detail = problem(lines) or ' '.join(rest.split())
        message = (title.rstrip(' .') + ('. ' + detail if detail else ''))[:MESSAGE]
        extra = {k: v.lower() for k in ('severity', 'category') if (v := field(rest, k))}
        out += [{'file': f, 'start_line': a, 'end_line': b, 'message': message, 'finding': n, **extra}
                for f, a, b in anchors]
    return out


def resolve(path, files, changed=()):
    """The file at head that path names: itself, without a diff 'a/' or 'b/' prefix, or the one file whose path
    ends with '/path' (when several do, the one changed file among them); None when there is none or it stays
    ambiguous."""
    if path in files:
        return path
    if path[:2] in ('a/', 'b/') and path[2:] in files:
        return path[2:]
    tails = [f for f in files if f.endswith('/' + path)]
    if len(tails) > 1:
        tails = [f for f in tails if f in changed]
    return tails[0] if len(tails) == 1 else None


def anchor(found, repo, head, base=None):
    """(kept, dropped): the entries whose file exists at head, with the path as it is there and lines clamped to
    the file (a binary file, submodule or empty file has line 1 only), and the entries dropped for a missing file.
    With base, a bare name shared by several files resolves to the one changed in base...head. Read-only git
    object reads; ContextError (a ValueError) if head or base is not a commit."""
    head = context.commit(repo, head, 'head')
    files = set(context.git(repo, 'ls-tree', '-r', '-z', '--name-only', head).split('\0')) - {''}
    changed = set(context.git(repo, 'diff', '--no-ext-diff', '--name-only', '-z',
                              f"{context.commit(repo, base, 'base')}...{head}").split('\0')) if base else set()
    lengths, kept, dropped = {}, [], []
    for item in found:
        path = resolve(clean(item['file']), files, changed)
        if path is None:
            dropped.append(item)
            continue
        if path not in lengths:
            try:
                text = context.blob(repo, head, path)
            except context.ContextError:
                text = None
            lengths[path] = max(1, text.count('\n') + (not text.endswith('\n')) if text else 1)
        start = min(max(1, item['start_line']), lengths[path])
        end = min(max(start, item['end_line']), lengths[path])
        kept.append({**item, 'file': path, 'start_line': start, 'end_line': end})
    return kept, dropped


def reviewbench(found, pr, agent):
    """The ReviewBench agent-contract JSON object: {pr: {repo, pr_number, base, head}, agent, findings}. pr holds
    what is known (pr.json fields; missing ones are None); a numeric pr_number string becomes an int."""
    number = pr.get('pr_number')
    if isinstance(number, str) and number.isdigit():
        number = int(number)
    return {'pr': {'repo': pr.get('repo'), 'pr_number': number, 'base': pr.get('base'), 'head': pr.get('head')},
            'agent': agent,
            'findings': [{'file': f['file'], 'start_line': f['start_line'], 'end_line': f['end_line'],
                          'message': f['message'], 'producer': agent} for f in found]}
