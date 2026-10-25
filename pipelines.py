"""Pipeline plans for the Ringo harness: which passes run, what each one sees, and how the replies become
the final result. Pure functions, no I/O apart from reading prompt files; harness.py makes the calls.

A plan is JSON data:

    {"version": 1, "kind": "review", "role": "You review code changes.",
     "passes": [{"id": "review", "prompt": "review.md", "reply": "findings"}]}

Each pass has an `id`, a `prompt` file (in prompts/, or in the plan's `prompts_dir`) and a `reply` type:
- `findings`: a review whose findings start with `### 1. Title` headings;
- `verdicts`: one line per finding, `<id> KEEP|REFUTED|DUP <id>|OPEN - <evidence>`, plus `<id> CUT "<sentence>"`;
- `issues`: an audit that replies NONE or lists issues;
- `proposal`: a design proposal (the result is the last proposal that ran);
- `text`: any other non-empty reply (a table of numbers, notes).
Optional fields: `inputs` (ids of earlier passes whose replies this pass reads), `batch` (verdicts only: findings
per call; each findings reply then holds at most MAX_FINDINGS findings, so the checker's calls are known from the
plan, never from what a model writes), `when` (the id of an earlier `issues` pass; this pass runs only if that pass found issues), and `model`
(a model for this pass on a claude-style backend, e.g. a cheaper one for checker batches). A plan never names a
backend: the verdicts pass can run on a second one only through harness --checker-backend, set by a person. The built-in plans are
plans/review.json and plans/design.json.

Every pass sees the same shared material (in the system prompt, so it is cached); its message holds only its
instruction and the replies it lists. The first pass runs alone to write the cache, then each wave of passes whose
inputs are ready runs in parallel, except under a dollar cap. Each backend and per-pass model warms separately. A checker
always runs, and at least one output pass must run unconditionally.

Review assembly keeps findings verbatim, in source order. With two or more `findings` passes a `verdicts` pass
is required, and every finding needs a verdict: KEEP is kept (minus any CUT sentences), REFUTED is dropped, OPEN
becomes an open question, and DUP is dropped only when its target is kept. A missing verdict, or a duplicate of a
finding that was not kept, asks the checker once more (harness.py) and otherwise fails the run.
"""
import json
import re
from pathlib import Path

PROMPTS = Path(__file__).resolve().parent / 'prompts'
PLANS = Path(__file__).resolve().parent / 'plans'
REPLIES = ('findings', 'verdicts', 'issues', 'proposal', 'text')
PASS_KEYS = {'id', 'prompt', 'reply', 'inputs', 'batch', 'when', 'model'}
PLAN_KEYS = {'version', 'kind', 'role', 'passes', 'prompts_dir'}
ID = r'[A-Za-z][\w-]*'
MAX_FINDINGS = 50  # per findings reply in a batched plan: its checker makes at most ceil(readers x 50 / batch) calls
HEAD = re.compile(r'^#{2,4}\s*\**\s*[A-Za-z]{0,2}(\d+)\s*[.):]\s*(.*)$', re.M)
SECTION = re.compile(r'^#{2,4}[ \t]*\**[ \t]*(open questions|not checked)'
                     r'(?:[ \t]*\([^\n)]*\))?[ \t]*\**[ \t]*:?[ \t]*$', re.M | re.I)
VERDICT = re.compile(rf'^[^\w\n]*({ID}\.\d+)[^\w\n]+(KEEP|REFUTED|DUP|OPEN)\b(.*)$', re.M)
FINDING_ID = re.compile(rf'\b{ID}\.\d+')
CUT = re.compile(rf'^[^\w\n]*({ID}\.\d+)[^\w\n]+CUT\b[^\w\n"]*"(.+)"[^\w\n]*$', re.M)


class NeedVerdicts(Exception):
    """Findings that still need a verdict; harness.py asks the checker once more."""

    def __init__(self, ids, reason):
        super().__init__(f'{reason}: {", ".join(ids)}')
        self.ids, self.reason = list(ids), reason


def load_plan(path=None, kind='review'):
    """The plan in the JSON file at path (default: the built-in plans/<kind>.json), validated."""
    path = Path(path) if path else PLANS / f'{kind}.json'
    plan = json.loads(path.read_text(encoding='utf-8'))
    if not isinstance(plan, dict):
        raise ValueError('a plan is a JSON object')
    if plan.get('kind') != kind:
        raise ValueError(f'a {kind} task needs a {kind} plan')
    base = (path.parent / plan['prompts_dir']).resolve() if isinstance(plan.get('prompts_dir'), str) else PROMPTS
    return validate(plan, base)


def validate(plan, prompts_dir):
    """The plan with each pass's prompt text loaded; ValueError on anything malformed."""
    unknown = set(plan) - PLAN_KEYS
    if unknown or plan.get('version') != 1 or plan.get('kind') not in ('review', 'design'):
        raise ValueError(f'a plan needs version 1 and kind review or design; unknown keys: {sorted(unknown)}')
    if not isinstance(plan.get('role'), str) or not plan['role'].strip():
        raise ValueError('a plan needs a role line')
    passes = plan.get('passes')
    if not isinstance(passes, list) or not passes:
        raise ValueError('a plan needs at least one pass')
    seen, kinds = {}, []
    for p in passes:
        if not isinstance(p, dict) or set(p) - PASS_KEYS:
            raise ValueError(f'pass {p!r}: unknown keys')
        pid = p.get('id')
        if not isinstance(pid, str) or not re.fullmatch(ID, pid) or pid in seen:
            raise ValueError(f'pass id {pid!r} is missing, malformed or repeated')
        if p.get('reply') not in REPLIES:
            raise ValueError(f'pass {pid}: reply must be one of {REPLIES}')
        inputs = p.setdefault('inputs', [])
        if not isinstance(inputs, list) or any(not isinstance(i, str) or i not in seen for i in inputs):
            raise ValueError(f'pass {pid}: inputs must name earlier passes')
        if 'when' in p and (not isinstance(p['when'], str) or seen.get(p['when']) != 'issues' or p['reply'] == 'verdicts'):
            raise ValueError(f'pass {pid}: when must name an earlier issues pass; a verdicts pass always runs')
        if 'batch' in p and (p['reply'] != 'verdicts' or type(p['batch']) is not int or p['batch'] < 1):
            raise ValueError(f'pass {pid}: batch is a positive integer, for verdicts passes only')
        if 'model' in p and (not isinstance(p['model'], str) or not re.fullmatch(r'[\w.:-]+', p['model'])):
            raise ValueError(f'pass {pid}: model is a model name')
        name = p.get('prompt')
        if not isinstance(name, str) or not re.fullmatch(r'[\w.-]+\.md', name):
            raise ValueError(f'pass {pid}: prompt must be a .md file name')
        file = (Path(prompts_dir) / name).resolve()
        if file.parent != Path(prompts_dir).resolve() or not file.is_file():
            raise ValueError(f'pass {pid}: prompt {name} not found in {prompts_dir}')
        p['text'] = file.read_text(encoding='utf-8').strip()
        seen[pid] = p['reply']
        kinds.append(p['reply'])
    readers, checkers = kinds.count('findings'), kinds.count('verdicts')
    if plan['kind'] == 'review' and (readers == 0 or checkers > 1 or (readers > 1 and not checkers)):
        raise ValueError('a review plan needs one or more findings passes, and one verdicts pass when there are several')
    if plan['kind'] == 'review' and checkers and kinds.index('verdicts') < len(kinds) - 1 - kinds[::-1].index('findings'):
        raise ValueError('the verdicts pass must come after every findings pass')
    output = 'findings' if plan['kind'] == 'review' else 'proposal'
    if not any(p['reply'] == output and 'when' not in p for p in passes):
        raise ValueError(f'a {plan["kind"]} plan needs a {output} pass without when')
    return plan


def waves(plan):
    """Passes grouped into waves: the first pass alone (it writes the cache), then each set whose inputs are ready."""
    passes = plan['passes']
    done, out = {passes[0]['id']}, [[passes[0]]]
    rest = passes[1:]
    while rest:
        ready = [p for p in rest if all(i in done for i in p['inputs']) and p.get('when', passes[0]['id']) in done
                 and not any(q['reply'] == 'findings' for q in rest if q is not p and p['reply'] == 'verdicts')]
        if not ready:  # validate() rules out cycles; this guards the invariant
            raise ValueError('passes cannot be ordered')
        out.append(ready)
        done |= {p['id'] for p in ready}
        rest = [p for p in rest if p['id'] not in done]
    return out


def unfenced(text):
    """Unwrap one closed whole-reply fence; mask inner fences through their close or EOF."""
    opening = re.compile(r'^[ \t]{0,3}(`{3,}|~{3,})[^\n]*$', re.M)
    def spans(value):
        cursor = 0
        while match := opening.search(value, cursor):
            marker = match.group(1)
            close = re.compile(r'^[ \t]{0,3}' + re.escape(marker[0]) + '{' + str(len(marker)) + r',}[ \t]*$', re.M)
            end = close.search(value, match.end() + 1)
            if end:
                yield match.start(), match.end(), end.start(), end.end(), True
                cursor = end.end()
            else:
                yield match.start(), match.end(), len(value), len(value), False
                break
    text = text.strip()
    blocks = list(spans(text))
    if blocks and blocks[0][0] == 0 and blocks[0][3] == len(text) and blocks[0][4]:
        text = text[blocks[0][1]:blocks[0][2]].strip('\n')
    masked = list(text)
    for start, _, _, end, _ in spans(text):
        masked[start:end] = ['\n' if c == '\n' else ' ' for c in text[start:end]]
    return text, ''.join(masked)


def findings(pid, text):
    """(preamble, {'<pid>.<n>': finding text}, sections) for a findings reply; n counts headings in order, so the
    model's own numbering never collides across passes. sections holds the open-questions and not-checked text."""
    text, masked = unfenced(text)
    marks = list(HEAD.finditer(masked))
    def depth(match):
        heading = match.group()
        return len(heading) - len(heading.lstrip('#'))
    terminal = [m for m in SECTION.finditer(masked) if not marks or depth(m) <= depth(marks[0])]
    ends = [m.start() for m in terminal]
    marks = [m for m in marks if not ends or m.start() < ends[0]]
    found = {}
    for k, m in enumerate(marks):
        stop = min([s for s in ends if s > m.start()] + ([marks[k + 1].start()] if k + 1 < len(marks) else [len(text)]))
        found[f'{pid}.{k + 1}'] = text[m.start():stop].strip()
    sections = {}
    for m in terminal:
        nxt = min([s for s in ends if s > m.start()] + [h.start() for h in marks if h.start() > m.start()] + [len(text)])
        sections[m.group(1).lower()] = text[m.end():nxt].strip()
    stop = min([m.start() for m in marks[:1]] + ends + [len(text)])
    return text[:stop].strip(), found, sections


def all_findings(plan, replies):
    """Every finding of every findings pass that ran, in source order."""
    out = {}
    for p in plan['passes']:
        if p['reply'] == 'findings' and p['id'] in replies:
            out.update(findings(p['id'], replies[p['id']])[1])
    return out


NO_DEFECTS = re.compile(r'^\W*no defects found\b', re.M | re.I)


def check_reply(plan, p, text):
    """ValueError if a reply has the wrong shape for its pass (the harness then asks once more). In a plan with a
    checker, a findings reply must use the headings or say "No defects found", so no reviewer is silently lost. In
    a plan with a batched checker it may hold at most MAX_FINDINGS findings: the checker makes one call per batch,
    so this keeps its call count a plan-time number instead of whatever a runaway reply produces."""
    if not text.strip():
        raise ValueError(f'pass {p["id"]}: empty reply')
    if p['reply'] == 'verdicts' and not VERDICT.search(text):
        raise ValueError(f'pass {p["id"]}: no verdict lines')
    checked = any(q['reply'] == 'verdicts' for q in plan['passes'])
    if p['reply'] == 'findings' and checked:
        masked = unfenced(text)[1]
        headings = list(HEAD.finditer(masked))
        if len(findings(p['id'], text)[1]) != len(headings):
            raise ValueError(f'pass {p["id"]}: finding headings appear after a terminal section')
        if not headings and not NO_DEFECTS.search(masked):
            raise ValueError(f'pass {p["id"]}: no finding headings and no "No defects found" line')
        if len(headings) > MAX_FINDINGS and any(q['reply'] == 'verdicts' and q.get('batch') for q in plan['passes']):
            raise ValueError(f'pass {p["id"]}: {len(headings)} findings, more than the {MAX_FINDINGS} a batched '
                             f'checker plan allows per reply')


def has_issues(text):
    """False when an issues reply says NONE."""
    return not bool(re.fullmatch(r'[^\w\n]*NONE[^\w\n]*(?:[-–—:] [^\n]*)?', text.strip(), re.I))


def instruction(plan, p, replies):
    """Declared input replies; checker findings already travel in its batch listing."""
    kinds = {q['id']: q['reply'] for q in plan['passes']}
    blocks = [f'=== Reply of pass {i} ===\n{replies[i]}' for i in p['inputs'] if i in replies
              and not (p['reply'] == 'verdicts' and kinds[i] == 'findings')]
    return '\n\n'.join([p['text'], *blocks])


def requests(plan, p, replies):
    """The messages for one pass: one, or one per batch of findings for a batched verdicts pass."""
    head = instruction(plan, p, replies)
    if p['reply'] != 'verdicts':
        return [head]
    items = list(all_findings(plan, replies).items())
    if not items:
        return []
    size = p.get('batch') or len(items)
    index = '\n'.join(f'{fid}: {body.splitlines()[0].lstrip("#* ")}' for fid, body in items)
    out = []
    for k in range(0, len(items), size):
        batch = items[k:k + size]
        listing = '\n\n'.join(f'[{fid}]\n{body}' for fid, body in batch)
        out.append(f'{head}\n\nGive a verdict for each finding in this batch: {", ".join(f for f, _ in batch)}.\n'
                   f'Every finding, for DUP targets:\n{index}\n\n=== Findings in this batch ===\n{listing}')
    return out


def combine(plan, p, replies, texts):
    """One verdicts reply from its batch replies, keeping each batch's verdicts for that batch's findings only."""
    if p['reply'] != 'verdicts':
        return texts[0]
    items = list(all_findings(plan, replies))
    size = p.get('batch') or len(items) or 1
    kept = []
    for k, text in enumerate(texts):
        mine = set(items[k * size:(k + 1) * size])
        kept.append(scope_verdicts(text, mine))
    return '\n'.join(kept)


def scope_verdicts(text, ids):
    """Only the verdict and CUT lines for the findings this request was allowed to judge."""
    return '\n'.join(m.group(0) for pattern in (VERDICT, CUT) for m in pattern.finditer(text) if m.group(1) in ids)


def repair_request(plan, p, replies, need):
    """The message asking the checker again for the findings in need (a NeedVerdicts)."""
    items = all_findings(plan, replies)
    listing = '\n\n'.join(f'[{fid}]\n{items[fid]}' for fid in need.ids if fid in items)
    return (f'{instruction(plan, p, replies)}\n\nThese findings still need a verdict ({need.reason}). Judge each on its own merits; '
            f'give DUP only for a finding that repeats one you KEEP.\n\n=== Findings ===\n{listing}')


def verdicts(text):
    """{id: (verdict, dup target)} and {id: [sentences to cut]}; a later line for the same id wins."""
    out = {}
    for m in VERDICT.finditer(text):
        target = FINDING_ID.search(m.group(3)) if m.group(2) == 'DUP' else None
        out[m.group(1)] = (m.group(2), target.group(0) if target else None)
    cuts = {}
    for m in CUT.finditer(text):
        cuts.setdefault(m.group(1), []).append(m.group(2))
    return out, cuts


def assemble(plan, replies):
    """The final result text (and stats) from the replies; NeedVerdicts if the checker must be asked again."""
    if plan['kind'] == 'design':
        ran = [p for p in plan['passes'] if p['reply'] == 'proposal' and p['id'] in replies]
        if not ran:
            raise ValueError('no proposal pass ran')
        return replies[ran[-1]['id']].strip(), {'selected': ran[-1]['id']}
    readers = [p for p in plan['passes'] if p['reply'] == 'findings' and p['id'] in replies]
    if not readers:
        raise ValueError('no findings pass ran')
    checker = next((p for p in plan['passes'] if p['reply'] == 'verdicts'), None)
    if checker is None:
        return replies[readers[0]['id']].strip(), {'kept': None}
    pre, sections, items = '', {}, {}
    for p in readers:
        head, found, secs = findings(p['id'], replies[p['id']])
        pre = pre or '\n'.join(line for line in head.splitlines() if not NO_DEFECTS.search(line)).strip()
        items.update(found)
        for k, v in secs.items():
            sections.setdefault(k, []).extend(line for line in v.splitlines() if line.strip())
    verdict, cuts = verdicts(replies.get(checker['id'], ''))
    missing = [f for f in items if f not in verdict]

    orphan = [f for f, (v, target) in verdict.items() if f in items and v == 'DUP'
              and (target not in items or verdict.get(target, (None,))[0] != 'KEEP')]
    if missing or orphan:
        reason = ' and '.join(reason for ids, reason in [(missing, 'no verdict'),
                              (orphan, 'duplicate of a finding that was not kept')] if ids)
        raise NeedVerdicts(missing + orphan, reason)
    kept, opened, uncut = [], [], 0
    for fid, body in items.items():
        v = verdict[fid][0]
        if v == 'KEEP':
            for sentence in cuts.get(fid, []):
                hits = list(re.finditer(rf'(?<!\S){re.escape(sentence)}(?!\S)', body))
                if len(hits) != 1:
                    uncut += 1
                    continue
                i, j = hits[0].span()
                j += body[i - 1:i] == ' ' and body[j:j + 1] == ' '
                body = body[:i] + body[j:]
            kept.append(body)
        elif v == 'OPEN':
            opened.append(HEAD.match(body).group(2).strip() if HEAD.match(body) else body.splitlines()[0])
    if kept and NO_DEFECTS.search(pre):
        pre = '\n'.join(line for line in pre.splitlines() if not NO_DEFECTS.search(line))
    out = [pre, ''] if pre else []
    for n, body in enumerate(kept, 1):
        out += [HEAD.sub(lambda m: f'### {n}. {m.group(2)}', body, count=1), '']
    if not kept:
        out += ['No defects found that survived checking.', '']
    seen_lines = list(dict.fromkeys(sections.get('open questions', [])))
    questions = [f'- {q}' for q in opened] + [line if line.startswith('-') else f'- {line}'
                 for line in seen_lines if line.strip('- ').lower() != 'none']
    out += ['### Open questions (not findings)', *(questions or ['none'])]
    out += ['', '### Not checked', *(list(dict.fromkeys(sections.get('not checked', []))) or ['n/a'])]
    return '\n'.join(out).strip(), {'kept': len(kept), 'open': len(opened), 'findings': len(items), 'cuts_not_found': uncut}
