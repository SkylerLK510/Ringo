"""Ringo eval runner: runs arms (a pipeline plan on a backend) over a pool of cases, grades the outputs blind,
and decides, fail-closed, whether a candidate beats its comparators, under guardrails against overfitting.

Store (one folder; whoever can read it can read the answer keys):
  cases.json, cases/<id>/material.txt, cases/<id>/gold.md   the pool; each gold file carries a canary line
  splits.json                                               dev / validation / test, by repository group
  ledger.jsonl                                              append-only, hash-chained record of every step
  runs/<digest>/                                            spec, outputs, traces, grades, scores
  keys/<digest>.json                                        blind id -> slot, kept outside the graded bundles

Guardrails:
- dev is for tuning. Validation caps submissions across campaigns sharing cases and releases aggregate scores
  to the operator; campaign proposers receive only a decision word. The store's policy (the lifetime error budget and the validation cap) is fixed in the ledger at the first
  split; no experiment can loosen it. The test split is sealed: one decision, then its cases are retired and can never be test again. Cases
  whose scores were released (exposed) or retired are forced into dev when splits are rebuilt, and no case ever
  moves away from dev: a dev case stays dev, a validation case never enters test. Every check-then-write step holds
  a store-wide lock across processes, and a test decision retires its cases in the same ledger record.
- Every input that affects a result (the spec, plans and their prompts, the harness, pipeline, evaluator and
  backend code, the grading prompt, every case file and its repository group, the split file, and each backend's
  resolved executable, content hash and version) is hashed into the digest at freeze; each later step re-checks
  it, so nothing can change after preregistration.
- Paired per-case comparisons, clustered by repository group: a cluster-robust t bound and a cluster bootstrap
  bound must both pass, for every grader, against every comparator. A test-split decision spends
  alpha_j = A / (j (j + 1)) for the j-th such decision ever made from this store (never reset; the sum over all
  decisions stays below A, the policy's error budget, 0.05 by default).
- A failed or skipped slot, a missing grade, too few repository groups, or an unknown cost makes the decision
  inconclusive. Grading needs a finished run; every slot's review must be in the blind key and in every grader's
  grades. The test split needs graders from at least two provider families. A family is vouched for by a person:
  `trust` records an executable's real path and content hash with its family in the ledger, and only an exact
  match counts; any other command is custom, is never executed at freeze time, and counts toward no family.
- Decisions are advisory. Code cannot verify from the inside that an OS boundary keeps an improver away from the
  answer keys (anything it checks, the same user can fake), so no decision adopts anything: a person adopts a
  change after a sealed-test pass, ideally from a store that the improver's user cannot read.
- Budgets are per experiment and persistent: spending is rebuilt from the finished slots and the grading charges
  log, so a resumed experiment neither forgets nor double-counts, and it is checked before every attempt. A
  budget requires a declared fallback (unpriced_call_usd) for every attempt with no price, including a timeout.
- One run and one grading per experiment at a time (an exclusive lock), so no slot is paid for twice.

usage:
  python3 evaluate.py import-reviewbench --store S --corpus REVIEWBENCH_CHECKOUT --clones DIR [--max-chars N]
  python3 evaluate.py trust  --store S --backend NAME --family FAMILY   (vouch for a CLI you checked)
  python3 evaluate.py split  --store S [--seed N] [--dev ID [ID ...]] [--fractions 0.2,0.3,0.5] [--resplit]
                             [--alpha 0.05] [--max-validation-submissions 5]   (policy: first split only)
  python3 evaluate.py freeze --store S --spec SPEC.json
  python3 evaluate.py run    --store S --digest D [--parallel 3]
  python3 evaluate.py grade  --store S --digest D
  python3 evaluate.py score  --store S --digest D
  python3 evaluate.py decide --store S --digest D
  python3 evaluate.py void --store S --digest D --reason TEXT
"""
import argparse
import concurrent.futures
import contextlib
import fcntl
import functools
import hashlib
import json
import math
import os
import random
import re
import secrets
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

import backends
import harness
import pipelines

HERE = Path(__file__).resolve().parent
CODE = ('backends.py', 'harness.py', 'pipelines.py', 'evaluate.py', 'prompts/grade.md')
SPLITS = ('dev', 'validation', 'test')
RANK = {'dev': 0, 'validation': 1, 'test': 2}  # a case only ever moves toward dev
METRICS = ('T', 'W', 'U', 'R')  # gold defects found, invalid findings, T - W, recall; per review
SPEC_DEFAULTS = {'runs': 2, 'objective': 'quality', 'margins': {'U': 0.5, 'W': 0.25}, 'max_cost_ratio': 2.0,
                 'min_saving': 0.2, 'min_groups': 10, 'bootstrap': 10000, 'seed': 1, 'budget_usd': None,
                 'max_calls_per_review': 40, 'kind': 'review', 'sample': None, 'unpriced_call_usd': None,
                 'allow_tool_backends': False, 'grading_reserve_usd': 0, 'timeout_s': harness.TIMEOUT_S,
                 'grade_timeout_s': 1800}
POLICY_DEFAULTS = {'alpha': 0.05, 'max_validation_submissions': 5, 'min_superiority_U': 0.5,
                   'max_noninferiority_U': 0.5, 'max_margin_W': 0.25, 'max_cost_ratio': 2.0,
                   'min_saving': 0.2, 'min_bootstrap_tail': 1}  # fixed in the ledger at the first split
SPEC_KEYS = {'name', 'campaign', 'split', 'arms', 'candidate', 'comparators', 'graders', *SPEC_DEFAULTS}


class EvalError(Exception):
    """A guardrail refused the step, or the store is inconsistent."""


def outside_repo(path, what):
    """Refuse a store or workspace inside this repository: it holds answer keys and case material, and the
    repository is public, so one careless `git add` would publish them."""
    try:
        return harness.outside_repo(path, what)
    except ValueError as exc:
        raise EvalError(str(exc)) from None


def sha(data):
    return hashlib.sha256(data if isinstance(data, bytes) else data.encode('utf-8')).hexdigest()


# ---------- ledger ----------

def ledger(store):
    with locked(store):
        return _ledger(store)


def _ledger(store):
    """Every ledger record, after checking the hash chain and its head (ledger.head holds the hash of the last line,
    so an edited, removed or reordered line, the last one included, is caught). Someone who rewrites both files on
    purpose is not caught: keeping the store out of the improver's reach (another user, a container) is the defence."""
    path, prev, rows = Path(store) / 'ledger.jsonl', '0' * 64, []
    if path.exists():
        for n, line in enumerate(path.read_text(encoding='utf-8').splitlines()):
            rec = json.loads(line)
            if rec.get('seq') != n or rec.get('prev') != prev:
                raise EvalError(f'the ledger is broken at line {n + 1}; nothing can be decided from this store')
            prev = sha(line)
            rows.append(rec)
    head = Path(store) / 'ledger.head'
    if (rows or head.exists()) and (not head.exists() or head.read_text(encoding='utf-8').strip() != prev):
        raise EvalError('the ledger does not match its head; nothing can be decided from this store')
    return rows


def private_store(store):
    """Create the store private from the first instant; tighten an existing root before opening files."""
    path = outside_repo(store, 'store')
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    path.chmod(0o700)
    return path


_lock, _held = threading.RLock(), threading.local()


@contextlib.contextmanager
def locked(store):
    """An exclusive lock on the store, across processes (POSIX flock on store/.lock) and threads; re-entrant
    within a thread, so a locked step can call record()."""
    store = private_store(store)
    with _lock:
        if getattr(_held, 'depth', 0):
            _held.depth += 1
            try:
                yield
            finally:
                _held.depth -= 1
            return
        with open(Path(store) / '.lock', 'a', encoding='utf-8') as f:
            fcntl.flock(f, fcntl.LOCK_EX)
            _held.depth = 1
            try:
                yield
            finally:
                _held.depth = 0
                fcntl.flock(f, fcntl.LOCK_UN)


def record(store, kind, **fields):
    with locked(store):
        rows = ledger(store)
        prev = sha((Path(store) / 'ledger.jsonl').read_text(encoding='utf-8').splitlines()[-1]) if rows else '0' * 64
        rec = {'seq': len(rows), 'time': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), 'kind': kind, **fields,
               'prev': prev}
        line = json.dumps(rec, sort_keys=True)
        with open(Path(store) / 'ledger.jsonl', 'a', encoding='utf-8') as f:
            f.write(line + '\n')
        (Path(store) / 'ledger.head').write_text(sha(line) + '\n', encoding='utf-8')
        return rec


def prereg(store, digest):
    for rec in ledger(store):
        if rec['kind'] == 'prereg' and rec['digest'] == digest:
            return rec
    raise EvalError(f'no preregistration for {digest}')


@contextlib.contextmanager
def exclusive(path, what):
    """A non-blocking exclusive lock on path, held across processes; EvalError if someone else holds it."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, 'a', encoding='utf-8') as f:
        try:
            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise EvalError(f'another {what} of this experiment is in progress') from None
        try:
            yield
        finally:
            fcntl.flock(f, fcntl.LOCK_UN)


# ---------- cases and splits ----------

def cases(store):
    pool = json.loads((Path(store) / 'cases.json').read_text(encoding='utf-8'))['cases']
    if not isinstance(pool, dict) or any(
            not re.fullmatch(r'[\w][\w.-]*', cid) or not isinstance(meta, dict)
            or not isinstance(meta.get('group'), str) or not meta['group'].strip() for cid, meta in pool.items()):
        raise EvalError('each case needs a safe file name and a non-empty repository group')
    return pool


def gold_entries(store, cid):
    """One parser for imported answer-key lines and the older location-free format."""
    result = []
    pattern = re.compile(r'\*\*(G\d+)\*\*\s*(?:\(([^)]*)\))?\s*(?:`([^`]+):\d+-\d+`)?:?\s*(.*)')
    for line in (Path(store) / 'cases' / cid / 'gold.md').read_text(encoding='utf-8').splitlines():
        if match := pattern.search(line):
            result.append(dict(zip(('id', 'note', 'path', 'message'), match.groups())))
    return result


def gold_ids(store, cid):
    return sorted({e['id'] for e in gold_entries(store, cid)}, key=lambda g: int(g[1:]))


def format_gold(n, entry):
    return (f"- **G{n}** ({entry['severity']}, {entry['category']}, difficulty {entry['difficulty']}) "
            f"`{entry['file']}:{entry['start_line']}-{entry['end_line']}`: {entry['message']}")


def policy(store):
    """The store's guardrail policy, as fixed by its first split."""
    first = next((r for r in ledger(store) if r['kind'] == 'split'), None)
    return {**POLICY_DEFAULTS, **(first['policy'] if first else {})}


def burned(rows, besides=None):
    """Cases whose scores were released (exposure) or that a test decision retired, by any experiment other than
    the digest besides."""
    return ({c for r in rows if r['kind'] == 'exposure' and r['digest'] != besides for c in r['cases']}
            | {c for r in rows if r['kind'] == 'decision' and r['digest'] != besides for c in r.get('retired', [])})


def split(store, seed=1, forced_dev=(), fractions=(0.2, 0.3, 0.5), resplit=False, new_policy=None):
    """Assign whole repository groups to dev, validation and test. A case only ever moves toward dev: one that was
    ever in dev stays in dev, one that was ever in validation never enters test, and exposed, retired or forced
    cases pull their whole group into dev. Refuses to redo an existing split without resplit, which is itself
    recorded. The first split fixes the policy; later ones keep it."""
    store = Path(store)
    with locked(store):
        if (store / 'splits.json').exists() and not resplit:
            raise EvalError('splits already exist; --resplit rebuilds them (no case ever moves away from dev)')
        pool, rows = cases(store), ledger(store)
        first = next((r for r in rows if r['kind'] == 'split'), None)
        if resplit and open_test(rows):
            raise EvalError('another test experiment is open; decide it, or void it before resplitting')
        fixed = {**POLICY_DEFAULTS, **(first['policy'] if first else (new_policy or {}))}
        if len(fractions) != 3 or any(harness.valid_price(x) is None for x in fractions) \
                or not math.isclose(sum(fractions), 1.0):
            raise EvalError('split fractions must be three finite, non-negative numbers summing to one')
        if set(fixed) != set(POLICY_DEFAULTS) or harness.valid_price(fixed['alpha']) is None \
                or not 0 < fixed['alpha'] < 0.5 or type(fixed['max_validation_submissions']) is not int \
                or fixed['max_validation_submissions'] < 1:
            raise EvalError('policy needs 0 < alpha < 0.5 and a positive integer validation cap')
        if any(harness.valid_price(fixed[k]) is None for k in set(POLICY_DEFAULTS) - {'alpha', 'max_validation_submissions'}) \
                or fixed['max_cost_ratio'] <= 0 or not 0 <= fixed['min_saving'] < 1 or fixed['min_bootstrap_tail'] < 1:
            raise EvalError('policy bounds must be finite and valid')
        if set(forced_dev) - set(pool):
            raise EvalError('forced dev cases must exist in the case pool')
        dead = burned(rows) | set(forced_dev)
        history = {}
        for r in rows:
            if r['kind'] == 'split':
                for cid, where in r['assignment'].items():
                    history[cid] = min(history.get(cid, RANK['test']), RANK[where])
        groups = {}
        for cid, meta in pool.items():
            groups.setdefault(meta['group'], []).append(cid)
        out = {s: [] for s in SPLITS}
        for g in sorted(groups, key=lambda g: sha(f'{seed}:{g}')):
            members = groups[g]
            ceiling = 0 if dead & set(members) else min(history.get(c, RANK['test']) for c in members)
            allowed = [s for s in SPLITS if RANK[s] <= ceiling]
            n = sum(len(v) for v in out.values()) + len(members)
            target = min(allowed, key=lambda s: (len(out[s]) + len(members)) / n - fractions[SPLITS.index(s)])
            out[target] += members
        doc = {'seed': seed, 'fractions': list(fractions), **{s: sorted(v) for s, v in out.items()}}
        text = json.dumps(doc, indent=1)
        (store / 'splits.json').write_text(text, encoding='utf-8')
        record(store, 'split', splits_sha256=sha(text), sizes={s: len(v) for s, v in out.items()}, resplit=resplit,
               policy=fixed, assignment={c: s for s, v in out.items() for c in v})
        return doc


def splits_doc(store):
    """splits.json, after checking it is the file the latest split record describes (an edited split is refused)."""
    rows = [r for r in ledger(store) if r['kind'] == 'split']
    text = (Path(store) / 'splits.json').read_text(encoding='utf-8')
    if not rows or sha(text) != rows[-1]['splits_sha256']:
        raise EvalError('splits.json does not match the ledger; only evaluate.py split may change it')
    return json.loads(text)


def split_cases(store, name, spec=None):
    """The split's case ids; for a spec with a sample, a seeded random subset of them."""
    ids = splits_doc(store)[name]
    if spec and spec.get('sample') and spec['sample'] < len(ids):
        ids = sorted(random.Random(spec['seed']).sample(ids, spec['sample']))
    return ids


# ---------- spec, digest, freeze ----------

def load_spec(path):
    spec = json.loads(Path(path).read_text(encoding='utf-8'))
    if not isinstance(spec, dict):
        raise EvalError('a spec must be a JSON object')
    unknown = set(spec) - SPEC_KEYS
    if unknown:
        raise EvalError(f'unknown spec keys: {sorted(unknown)}')
    spec = {**SPEC_DEFAULTS, **spec}
    if spec.get('split') not in SPLITS or not isinstance(spec.get('arms'), dict) or not spec['arms']:
        raise EvalError('a spec needs split (dev, validation or test) and arms')
    if any(not re.fullmatch(pipelines.ID, name) or not isinstance(arm, dict)
           or set(arm) - {'plan', 'backend'} or not isinstance(arm.get('backend'), str)
           or (arm.get('plan') is not None and not isinstance(arm['plan'], str))
           for name, arm in spec['arms'].items()):
        raise EvalError('each arm needs a safe id and {"plan": FILE or null, "backend": BACKEND}')
    spec.setdefault('comparators', [])
    if not isinstance(spec.get('candidate'), str) or spec['candidate'] not in spec['arms'] \
            or not isinstance(spec['comparators'], list) or not all(isinstance(c, str) for c in spec['comparators']) \
            or not set(spec['comparators']) <= set(spec['arms']) - {spec['candidate']} \
            or (not spec['comparators'] and spec['split'] != 'dev'):
        raise EvalError('candidate must be an arm and comparators other arms (only a dev run may have none)')
    if spec['sample'] is not None and (spec['split'] != 'dev' or type(spec['sample']) is not int or spec['sample'] < 1):
        raise EvalError('sample (a number of cases) is for dev runs only')
    if not isinstance(spec.get('graders'), list) or not spec['graders'] \
            or not all(isinstance(g, str) and g.strip() for g in spec['graders']) \
            or len(set(spec['graders'])) != len(spec['graders']):
        raise EvalError('a spec needs at least one grader backend')
    for k, minimum in (('min_groups', 10), ('bootstrap', 1), ('max_calls_per_review', 1)):
        if type(spec[k]) is not int or spec[k] < minimum:
            raise EvalError(f'{k} must be an integer >= {minimum}')
    if type(spec['seed']) is not int or spec['kind'] != 'review':
        raise EvalError('seed must be an integer and evaluation kind must be review')
    if not isinstance(spec['margins'], dict) or set(spec['margins']) != {'U', 'W'} \
            or any(harness.valid_price(x) is None for x in spec['margins'].values()):
        raise EvalError('margins must contain finite, non-negative U and W values')
    if harness.valid_price(spec['max_cost_ratio']) is None or spec['max_cost_ratio'] <= 0 \
            or harness.valid_price(spec['min_saving']) is None or not 0 <= spec['min_saving'] < 1:
        raise EvalError('max_cost_ratio must be positive and 0 <= min_saving < 1')
    for k in ('budget_usd', 'unpriced_call_usd'):
        if spec[k] is not None and harness.valid_price(spec[k]) is None:
            raise EvalError(f'{k} must be a finite, non-negative number')
    if spec['budget_usd'] is not None and spec['unpriced_call_usd'] is None:
        raise EvalError('a budget needs unpriced_call_usd: any backend can fail to report cost, including a timeout')
    if type(spec['allow_tool_backends']) is not bool:
        raise EvalError('allow_tool_backends must be a bool')
    harness.check_backends([*(a['backend'] for a in spec['arms'].values()), *spec['graders']], spec['allow_tool_backends'])
    for k in ('timeout_s', 'grade_timeout_s'):
        if harness.valid_price(spec[k]) is None or spec[k] <= 0:
            raise EvalError(f'{k} must be finite and positive')
    reserve = harness.valid_price(spec['grading_reserve_usd'])
    if reserve is None or (reserve and (spec['budget_usd'] is None or reserve > spec['budget_usd'])):
        raise EvalError('grading_reserve_usd must be between zero and budget_usd')
    if spec['objective'] not in ('quality', 'cost') or type(spec['runs']) is not int or spec['runs'] < 1:
        raise EvalError('objective is quality or cost, and runs a positive integer')
    base = Path(path).resolve().parent
    for arm in spec['arms'].values():
        if arm.get('plan'):
            arm['plan'] = str((base / arm['plan']).resolve())
    spec.setdefault('campaign', spec.get('name', 'default'))
    return spec


def file_sha(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            h.update(chunk)
    return h.hexdigest()


def family(name, store):
    """A backend's provider family: the one a person vouched for its exact executable (see trust), else custom."""
    return identity(name, store)['family']


def trusted(store, path, digest_):
    """The family a person vouched for this exact executable (path and content hash), or None."""
    hits = [r for r in ledger(store) if r['kind'] == 'trust' and r['path'] == path and r['sha256'] == digest_]
    return hits[-1]['family'] if hits else None


def trust(store, name, fam):
    """Record that a person checked the executable behind backend `name` and vouches it is the `fam` CLI. The record
    holds the executable's real path and content hash, so an update or a renamed stand-in is not trusted."""
    exe = shutil.which(harness.backend(name)['argv'][0])
    if not exe or not re.fullmatch(r'[a-z][a-z0-9-]*', fam) or fam == 'custom':
        raise EvalError('trust needs an executable on PATH and a family name (lower case, not "custom")')
    real = str(Path(exe).resolve())
    return record(store, 'trust', path=real, sha256=file_sha(real), family=fam)


def identity(name, store):
    """What a backend name resolves to, frozen into the digest: argv, style, every file on its command line (the
    executable, any script it runs, any file attached to an option as --opt=FILE) with its content hash, and,
    only for an executable a person vouched for with `trust`, its family and its --version, run isolated. Any
    other command is never executed here (--version could start real, paid work) and its family is custom.
    Files a backend reads that are not on its command line are not frozen."""
    b = harness.backend(name)
    exe = shutil.which(b['argv'][0])
    if not exe or (os.sep in b['argv'][0] and not os.path.isabs(b['argv'][0])):
        raise EvalError(f'backend {name!r}: use an executable on PATH or an absolute executable path')
    files = {}
    for arg in filter(None, [exe, *b['argv'][1:]]):
        attached = arg.split('=', 1)[1] if arg.startswith('-') and '=' in arg else None
        for cand in filter(None, (arg, attached)):
            if os.path.isabs(cand) and os.path.isfile(cand):
                files[str(Path(cand).resolve())] = file_sha(cand)
    real = str(Path(exe).resolve()) if exe else None
    fam = trusted(store, real, files.get(real)) if real else None
    version = None
    if fam:
        try:
            version = backends.run_isolated([real, '--version'], '', 30).strip()[:200] or None
        except (backends.JudgeError, OSError, subprocess.TimeoutExpired):
            version = None
    return {'name': name, 'argv': b['argv'], 'style': b['style'], 'files': files, 'version': version,
            'family': fam or 'custom'}


def digest(store, spec):
    """The hash of everything that can change a result."""
    store, pool = Path(store), cases(store)
    parts = [json.dumps(spec, sort_keys=True), sha((store / 'splits.json').read_bytes())]
    canaries = store / 'canaries.json'
    parts.append('canaries:' + (sha(canaries.read_bytes()) if canaries.exists() else 'absent'))
    for name in CODE:
        parts.append(sha((HERE / name).read_bytes()))
    for arm in sorted(spec['arms']):
        plan = pipelines.load_plan(spec['arms'][arm].get('plan'), spec['kind'])
        parts.append(json.dumps(plan, sort_keys=True))  # plan with every prompt's text loaded
    names = sorted({a['backend'] for a in spec['arms'].values()} | set(spec['graders']))
    parts.append(json.dumps([identity(n, store) for n in names], sort_keys=True))
    for cid in split_cases(store, spec['split'], spec):
        parts.append(f"{cid} {pool[cid]['group']}")
        for f in ('material.txt', 'gold.md'):
            parts.append(sha((store / 'cases' / cid / f).read_bytes()))
    return sha('\n'.join(parts))


def alpha(store, split_name):
    """One-sided error rate for a decision: the policy's budget A on dev and validation (selection, which a test
    decision must still confirm), A / (j (j + 1)) for the j-th test decision."""
    a = policy(store)['alpha']
    if split_name != 'test':
        return a
    j = 1 + sum(r['kind'] == 'decision' and r['split'] == 'test' for r in ledger(store))
    return a / (j * (j + 1))


def open_test(rows):
    closed = {r['digest'] for r in rows if r['kind'] == 'decision'}
    return any(r['kind'] == 'prereg' and r['split'] == 'test' and r['digest'] not in closed for r in rows)


def frozen_cases(store, registration):
    """The assignment at preregistration, even after an input or split changes."""
    rows = [r for r in ledger(store) if r['kind'] == 'split' and r['seq'] < registration['seq']]
    return sorted(c for c, split_ in rows[-1]['assignment'].items() if split_ == registration['split'])


def check_policy(spec, fixed, a):
    if spec['split'] == 'dev':
        return
    mu, mw = spec['margins']['U'], spec['margins']['W']
    loose = mw > fixed['max_margin_W']
    if spec['objective'] == 'quality':
        loose |= mu < fixed['min_superiority_U'] or spec['max_cost_ratio'] > fixed['max_cost_ratio']
    else:
        loose |= mu > fixed['max_noninferiority_U'] or spec['min_saving'] < fixed['min_saving']
    if loose or spec['bootstrap'] * a < fixed['min_bootstrap_tail']:
        raise EvalError('experiment gates are looser than the store policy or bootstrap tail is unresolved')


def void(store, d, reason):
    """Close an invalidated experiment without trusting changed inputs; conservatively burn its frozen test pool."""
    if not isinstance(reason, str) or not reason.strip():
        raise EvalError('void needs a non-empty reason')
    store = Path(store)
    prereg(store, d)  # refuse unknown identifiers before creating any lock paths
    with exclusive(store / 'runs' / d / '.run.lock', 'run'), exclusive(store / 'runs' / d / '.grade.lock', 'grading'):
        with locked(store):
            reg = prereg(store, d)
            if any(r['kind'] == 'decision' and r['digest'] == d for r in ledger(store)):
                raise EvalError('this experiment is already decided')
            record(store, 'decision', digest=d, split=reg['split'], result='inconclusive', void=True,
                   reasons=['voided: ' + reason.strip()], alpha=reg['alpha'],
                   retired=frozen_cases(store, reg) if reg['split'] == 'test' else [])
    return 'inconclusive'


def freeze(store, spec_path):
    store, spec = Path(store), load_spec(spec_path)
    with locked(store):  # the checks and the preregistration are one step for every process
        rows = ledger(store)
        if spec['split'] == 'test' and len({family(g, store) for g in spec['graders']} - {'custom'}) < 2:
            raise EvalError('the test split needs graders from at least two provider families, each vouched for with '
                            '`evaluate.py trust`')
        check_policy(spec, policy(store), alpha(store, spec['split']))
        if spec['split'] == 'test':
            if open_test(rows):
                raise EvalError('another test experiment is open; decide it, or void it if inputs changed')
            if burned(rows) & set(split_cases(store, 'test')):
                raise EvalError('this test split includes exposed or retired cases; rebuild splits with new cases first')
        if spec['split'] == 'validation':
            current, in_force, used = set(split_cases(store, 'validation')), set(), 0
            for r in rows:
                if r['kind'] == 'split':
                    in_force = {c for c, part in r['assignment'].items() if part == 'validation'}
                elif r['kind'] == 'prereg' and r['split'] == 'validation' and in_force & current:
                    used += 1
            cap = policy(store)['max_validation_submissions']
            if used >= cap:
                raise EvalError(f'this validation set has used its {cap} validation submissions; import fresh cases and resplit')
        d = digest(store, spec)
        if any(r['kind'] == 'prereg' and r['digest'] == d for r in rows):
            raise EvalError('this exact experiment is already preregistered')
        (store / 'runs' / d).mkdir(parents=True, exist_ok=True)
        (store / 'runs' / d / 'spec.json').write_text(json.dumps(spec, indent=1), encoding='utf-8')
        record(store, 'prereg', digest=d, split=spec['split'], campaign=spec['campaign'],
               alpha=alpha(store, spec['split']), spec=spec)
        return d


def checked(store, d):
    """The preregistered spec, after checking that no input changed since freeze."""
    spec = prereg(store, d)['spec']
    if digest(store, spec) != d:
        raise EvalError('an input changed since freeze (code, prompts, plans or cases); preregister again')
    return spec


# ---------- run ----------

def pending(where):
    """True if the slot's trace has an attempt that started and never settled (its cost is unknown)."""
    path, open_ = where / 'trace' / 'trace.jsonl', set()
    if path.exists():
        for line in path.read_text(encoding='utf-8').splitlines():
            rec = json.loads(line)
            (open_.add if rec['status'] == 'started' else open_.discard)((rec['step'], rec['attempt']))
    return bool(open_)


def slot_cost(where, unpriced_call_usd=None):
    """What a slot cost, None if any part is unknown. A finished slot reports its total; an unfinished one is rebuilt
    from its trace, one record per attempt: a paid attempt costs its reported price (or the declared price per call
    when it reports none), and an attempt that started but never finished makes the cost unknown."""
    if pending(where):
        return None
    for name in ('summary.json', 'failed.json'):
        if (where / name).exists():
            data = json.loads((where / name).read_text(encoding='utf-8'))
            u = (data.get('usage') or {}) if name == 'failed.json' else data
            if 'calls' not in u and (where / 'trace' / 'trace.jsonl').exists():
                break  # a failure recorded without a summary (e.g. interrupted): rebuild from the trace
            total = harness.valid_price(u.get('cost_usd') or 0.0)
            unpriced = u.get('unpriced_calls') or 0
            if total is None or (unpriced and unpriced_call_usd is None):
                return None
            return total + unpriced * (unpriced_call_usd or 0)
    path = where / 'trace' / 'trace.jsonl'
    if not path.exists():
        return 0.0
    started, total = set(), 0.0
    for line in path.read_text(encoding='utf-8').splitlines():
        rec = json.loads(line)
        key = (rec['step'], rec['attempt'])
        if rec['status'] == 'started':
            started.add(key)
            continue
        started.discard(key)
        if rec['status'] in ('ok', 'malformed', 'error'):
            price = harness.valid_price((rec.get('usage') or {}).get('cost_usd'))
            price = price if price is not None else unpriced_call_usd
            if price is None:
                return None
            total += price
    return None if started else total


def grade_charge(store, d, charge):
    record(store, 'grade_charge', digest=d, charge=charge)
    with open(Path(store) / 'runs' / d / 'grade-charges.jsonl', 'a', encoding='utf-8') as f:
        f.write(json.dumps(charge) + '\n')


def charges(store, d):
    """The grading charges of an experiment from its append-only charges file: one 'started' line before every
    grader attempt and one settled line after it. An attempt that never settled is returned with cost None."""
    path = Path(store) / 'runs' / d / 'grade-charges.jsonl'
    saved = [r['charge'] for r in ledger(store) if r['kind'] == 'grade_charge' and r['digest'] == d]
    written = [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines()] if path.exists() else []
    mismatch = bool(saved) and saved != written
    open_, settled = {}, []
    for ch in saved or written:
        key = (ch['case'], ch['grader'], ch['attempt'])
        if ch.get('status') == 'started':
            open_[key] = ch
        else:
            open_.pop(key, None)
            settled.append(ch)
    return settled + [{**ch, 'cost_usd': None} for ch in open_.values()] + ([{
        'case': '', 'grader': '', 'attempt': 0, 'cost_usd': None, 'status': 'changed charge log'}] if mismatch else [])


def cost(store, d, spec=None, active_slot=None):
    """(USD spent so far by an experiment's runs and grading, whether any of it is unknown). Rebuilt from the slot
    files and the charges log every time, so a resume neither forgets nor double-counts."""
    store, spec = Path(store), spec or prereg(store, d)['spec']
    total, unknown = 0.0, False
    seals = outcomes(store, d, 'slot')
    for c, a, r in slots(store, spec):
        seal = seals.get(f'{c}/{a}.{r}')
        x = seal.get('cost_usd') if seal and f'{c}/{a}.{r}' != active_slot else slot_cost(store / 'runs' / d / 'out' / c / f'{a}.{r}', spec['unpriced_call_usd'])
        unknown |= x is None
        total += x or 0.0
    for ch in charges(store, d):
        price = harness.valid_price(ch['cost_usd'])
        unknown |= price is None
        total += price or 0.0
    return total, unknown


def slots(store, spec):
    return [(c, a, r) for c in split_cases(store, spec['split'], spec) for a in sorted(spec['arms'])
            for r in range(1, spec['runs'] + 1)]


def outcomes(store, d, kind):
    return {r['item']: r for r in ledger(store) if r['kind'] == kind and r['digest'] == d}


def artifact_hash(where):
    names = ('result.md', 'summary.json', 'failed.json')
    return sha(json.dumps({n: file_sha(where / n) for n in names if (where / n).exists()}, sort_keys=True))


def unfinished_slots(store, d, spec):
    out = Path(store) / 'runs' / d / 'out'
    return [f'{c}/{a}.{r}' for c, a, r in slots(store, spec)
            if not any((out / c / f'{a}.{r}' / name).exists() for name in ('result.md', 'failed.json'))]


@contextlib.contextmanager
def settled(store, d, spec):
    root = Path(store) / 'runs' / d
    with exclusive(root / '.run.lock', 'run'), exclusive(root / '.grade.lock', 'grading'):
        if unfinished_slots(store, d, spec):
            raise EvalError('the run is not finished; run it first or void the experiment')
        for cid in split_cases(store, spec['split'], spec):
            if not list((root / 'out' / cid).glob('*/result.md')):
                continue
            for g in spec['graders']:
                target = root / 'grades' / sha(g)[:12] / f'{cid}.json'
                if not target.exists() and not target.with_suffix('.failed').exists():
                    raise EvalError('grading is not finished; grade it first or void the experiment')
        yield


def run(store, d, parallel=3, call_fn=harness.call, stop_on_failure=False):
    """Run every slot (case x arm x run) in a seeded random order, one invocation per experiment at a time. A failure
    is recorded and never re-run. With a budget, slots run one after another and each starts only if the
    experiment's spending (rebuilt from disk) is known and below the budget; its calls get the rest as their cap.
    Slots that cannot start are recorded as failed. Both make the decision inconclusive."""
    store, spec = Path(store), checked(store, d)
    out = store / 'runs' / d / 'out'
    todo = slots(store, spec)
    random.Random(spec['seed']).shuffle(todo)
    budget = spec['budget_usd']
    before = cost(store, d, spec)[0]
    stopped = threading.Event()

    def execute(slot):
        cid, arm, r = slot
        where = out / cid / f'{arm}.{r}'
        if (where / 'result.md').exists() or (where / 'failed.json').exists():
            return
        if (where / 'trace' / 'trace.jsonl').exists():  # interrupted earlier: paid for, never silently run again
            (where / 'failed.json').write_text(json.dumps({'error': 'interrupted before it finished'}), encoding='utf-8')
            return
        where.mkdir(parents=True, exist_ok=True)
        left = None
        if budget is not None:
            spent, unknown = cost(store, d, spec, active_slot=f'{cid}/{arm}.{r}')
            left = budget - spec['grading_reserve_usd'] - spent
            if unknown or left <= 0:
                (where / 'failed.json').write_text(json.dumps({'error': 'budget spent or unknown before this slot'}),
                                                   encoding='utf-8')
                return
        a = spec['arms'][arm]
        tracker = harness.Run(where / 'trace', max_calls=spec['max_calls_per_review'], max_cost=left,
                              unpriced_call_usd=spec['unpriced_call_usd'])
        def review_call(*args):
            text, usage = call_fn(*args)
            if leaked(store, text):
                error = EvalError('an answer-key canary appeared in a reviewer output')
                error.usage = usage
                raise error
            return text, usage

        try:
            text, stats = harness.run_plan(pipelines.load_plan(a.get('plan'), spec['kind']),
                                           (store / 'cases' / cid / 'material.txt').read_text(encoding='utf-8'),
                                           harness.backend(a['backend']), tracker, call_fn=review_call,
                                           timeout_s=spec['timeout_s'], scope=sha(f'{d}:{cid}:{arm}:{r}'))
            if leaked(store, text):
                raise EvalError('an answer-key canary appeared in a reviewer output')
        except Exception as exc:  # recorded, never retried: a rerun after seeing a failure is a selective rerun
            (where / 'failed.json').write_text(json.dumps({'error': f'{type(exc).__name__}: {exc}'[:500],
                                                           'usage': tracker.summary()}), encoding='utf-8')
        else:
            (where / 'summary.json').write_text(json.dumps({**tracker.summary(), **stats}), encoding='utf-8')
            (where / 'result.md').write_text(text + '\n', encoding='utf-8')

    def one(slot):
        cid, arm, r = slot
        item = f'{cid}/{arm}.{r}'
        where = out / item
        previous = outcomes(store, d, 'slot').get(item)
        if previous:
            if previous.get('artifact') == artifact_hash(where) and previous['status'] != 'started':
                return
            where.mkdir(parents=True, exist_ok=True)
            (where / 'result.md').unlink(missing_ok=True)
            (where / 'failed.json').write_text(json.dumps({'error': 'earlier outcome removed, changed or interrupted'}))
            if stop_on_failure:
                stopped.set()
            return
        if any((where / name).exists() for name in ('result.md', 'summary.json')):
            raise EvalError(f'unsealed slot artifacts for {item}; void this experiment instead of adopting them')
        record(store, 'slot', digest=d, item=item, status='started')
        if stopped.is_set():
            where.mkdir(parents=True, exist_ok=True)
            (where / 'failed.json').write_text(json.dumps({'error': 'not run: an earlier slot failed', 'skipped': True}))
        else:
            execute(slot)
        failed = (where / 'failed.json').exists()
        record(store, 'slot', digest=d, item=item, status='failed' if failed else 'result',
               artifact=artifact_hash(where), cost_usd=slot_cost(where, spec['unpriced_call_usd']))
        if failed and stop_on_failure:
            stopped.set()

    with exclusive(store / 'runs' / d / '.run.lock', 'run'):
        with concurrent.futures.ThreadPoolExecutor(1 if budget is not None else max(1, parallel)) as pool:
            list(pool.map(one, todo))
    failed = sorted(str(p.parent.relative_to(out)) for p in out.glob('*/*/failed.json'))
    record(store, 'run', digest=d, slots=len(todo), failed=failed, cost_usd=round(cost(store, d, spec)[0] - before, 6))
    return failed


# ---------- grade ----------

def parse_grades(text, ids, gold):
    """{review id: grade} from a grader reply: exactly one top-level JSON object that grades every review and
    repeats no key (backends.top_level_objects), so JSON quoted from a review cannot stand in for the grade.
    ValueError if there is none, several, or a grade is malformed."""
    matches = [o for o in backends.top_level_objects(text) if set(ids) == set(o) and not o.repeats_a_key]
    if len(matches) != 1:
        raise ValueError(f'{len(matches)} JSON objects grade every review; need exactly one')
    found, out = matches[0], {}
    for rid in ids:
        g = found[rid]
        if not isinstance(g, dict):
            raise ValueError(f'grade for {rid} is not an object')
        fields = ('found', 'invalid', 'valid_extra', 'style')
        if not {'found', 'invalid'} <= set(g):
            raise ValueError(f'grade for {rid} must contain found and invalid')
        lists = {k: g.get(k, []) for k in fields}
        if not all(isinstance(v, list) and all(isinstance(x, str) for x in v) for v in lists.values()):
            raise ValueError(f'grade for {rid} is malformed')
        unknown = set(lists['found']) - set(gold)
        if unknown:
            raise ValueError(f'grade for {rid} names unknown gold ids {sorted(unknown)}')
        out[rid] = {**lists, 'found': sorted(set(lists['found']), key=lambda x: int(x[1:]))}
    return out


def grade(store, d, call_fn=harness.call, timeout_s=None):
    """Grade every case's outputs with every grader, blind: fresh random ids, shuffled order, no arm names; the key
    goes to keys/, outside the run folder, and is saved before each case is sent. Needs a finished run (every slot
    has a result or a recorded failure). A malformed reply is asked once more; then that grade is missing. Grading
    stops when the experiment's budget (run cost plus grading cost) is spent or unknown. One grading per experiment
    runs at a time. The key file is always written, empty if no slot produced a review."""
    store, spec = Path(store), checked(store, d)
    with exclusive(store / 'runs' / d / '.grade.lock', 'grading'):
        _grade(store, d, spec, call_fn, spec['grade_timeout_s'] if timeout_s is None else timeout_s)


def _grade(store, d, spec, call_fn, timeout_s):
    out = store / 'runs' / d / 'out'
    unfinished = unfinished_slots(store, d, spec)
    if unfinished:
        raise EvalError(f'the run is not finished ({len(unfinished)} slots have no result); run it first')
    instructions = (HERE / 'prompts' / 'grade.md').read_text(encoding='utf-8').strip()
    keyfile = store / 'keys' / f'{d}.json'
    keyfile.parent.mkdir(parents=True, exist_ok=True)
    key_seals = [r for r in ledger(store) if r['kind'] == 'blind_key' and r['digest'] == d]
    if (keyfile.exists() and not key_seals) or (key_seals and (
            not keyfile.exists() or key_seals[-1]['artifact'] != file_sha(keyfile))):
        raise EvalError('the blind key is unsealed or changed; void this experiment')
    key = json.loads(keyfile.read_text(encoding='utf-8')) if keyfile.exists() else {}
    keyfile.write_text(json.dumps(key, indent=1), encoding='utf-8')
    if not key_seals:
        record(store, 'blind_key', digest=d, artifact=file_sha(keyfile))
    rng, graded_cost = secrets.SystemRandom(), 0.0
    for cid in split_cases(store, spec['split'], spec):
        results = sorted(f'{cid}/{p.parent.name}' for p in (out / cid).glob('*/result.md'))
        if not results:
            continue
        mine = {rid: slot for rid, slot in key.items() if slot.split('/')[0] == cid}
        if mine and sorted(mine.values()) != results:
            for g in spec['graders']:
                target = store / 'runs' / d / 'grades' / sha(g)[:12] / f'{cid}.failed'
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text('blind key no longer matches the outputs')
            continue
        if not mine:
            for slot in results:
                rid = f'r-{secrets.token_hex(4)}'
                while rid in key:
                    rid = f'r-{secrets.token_hex(4)}'
                key[rid] = mine[rid] = slot
            keyfile.write_text(json.dumps(key, indent=1), encoding='utf-8')
            record(store, 'blind_key', digest=d, artifact=file_sha(keyfile))
        order = list(mine)
        rng.shuffle(order)
        gold = gold_ids(store, cid)
        system = instructions + '\n\n' + harness.fence('MATERIAL', (store / 'cases' / cid / 'material.txt').read_text(encoding='utf-8'),
                               'untrusted change material') + '\n\n=== GROUND TRUTH ===\n' + (store / 'cases' / cid / 'gold.md').read_text(encoding='utf-8')
        reviews = '\n\n'.join(f'=== Review {rid} ===\n' + harness.fence('REVIEW', (out / mine[rid] / 'result.md').read_text(encoding='utf-8'), 'untrusted review text')
                              for rid in order)
        message = f'{instructions}\n\nReview ids: {", ".join(order)}\n\n{reviews}'
        for g in spec['graders']:
            target = store / 'runs' / d / 'grades' / sha(g)[:12] / f'{cid}.json'
            target.parent.mkdir(parents=True, exist_ok=True)
            item = f'{cid}/{sha(g)}'
            seal = outcomes(store, d, 'grade_outcome').get(item)
            if seal:
                if not target.exists() or seal.get('artifact') != file_sha(target):
                    target.unlink(missing_ok=True)
                    target.with_suffix('.failed').write_text('earlier grade removed, changed or interrupted')
                continue
            if target.exists():
                raise EvalError(f'unsealed grade for {item}; void this experiment instead of adopting it')
            if target.with_suffix('.failed').exists():
                continue
            if any(json.loads(p.read_text()).get('skipped') for p in out.glob('*/*/failed.json')):
                target.with_suffix('.failed').write_text('run stopped on failure')
                continue
            if any(ch['case'] == cid and ch['grader'] == g and ch['cost_usd'] is None and ch.get('status') == 'started'
                   for ch in charges(store, d)):  # interrupted earlier: paid for, never repeated for free
                (target.parent / f'{cid}.failed').write_text('an earlier attempt was interrupted', encoding='utf-8')
                continue
            previous = [ch['attempt'] for ch in charges(store, d) if ch['case'] == cid and ch['grader'] == g]
            if previous and max(previous) >= 2:
                target.with_suffix('.failed').write_text('two grading attempts already spent')
            for attempt in range(max(previous, default=0) + 1, 3):
                if spec['budget_usd'] is not None:
                    spent, unknown = cost(store, d, spec)
                    if unknown or spent >= spec['budget_usd']:
                        (target.parent / f'{cid}.failed').write_text('budget spent or unknown', encoding='utf-8')
                        break
                usage, text = None, None
                record(store, 'grade_outcome', digest=d, item=item, status='started')
                grade_charge(store, d, {'case': cid, 'grader': g, 'attempt': attempt, 'status': 'started'})
                try:
                    text, usage = call_fn(harness.backend(g), system, message, timeout_s)
                except (backends.JudgeError, subprocess.TimeoutExpired, OSError) as exc:
                    usage, text = ({'cost_usd': 0.0} if isinstance(exc, OSError) else getattr(exc, 'usage', None)), None
                    problem = str(exc)
                price = harness.valid_price((usage or {}).get('cost_usd'))
                price = price if price is not None else spec['unpriced_call_usd']
                grade_charge(store, d, {'case': cid, 'grader': g, 'attempt': attempt, 'status': 'settled', 'cost_usd': price})
                graded_cost += price or 0
                if text is not None:
                    try:
                        target.write_text(json.dumps(parse_grades(text, order, gold), indent=1), encoding='utf-8')
                        record(store, 'grade_outcome', digest=d, item=item, status='graded', artifact=file_sha(target))
                        break
                    except ValueError as exc:
                        problem = str(exc)
                if attempt == 2:
                    (target.parent / f'{cid}.failed').write_text(problem[:500], encoding='utf-8')
            if not target.exists():
                record(store, 'grade_outcome', digest=d, item=item, status='failed')
    record(store, 'grade', digest=d, graders=spec['graders'], cost_usd=round(graded_cost, 6))


# ---------- statistics ----------

def _beta_fraction(a, b, x):
    """Continued fraction for the incomplete beta integral, evaluated with modified Lentz iteration."""
    tiny = 1e-300
    c, d = 1.0, 1 - (a + b) * x / (a + 1)
    d = 1 / (d if abs(d) >= tiny else tiny)
    h = d
    for m in range(1, 10001):
        for coefficient in (m * (b - m) * x / ((a + 2 * m - 1) * (a + 2 * m)),
                            -(a + m) * (a + b + m) * x / ((a + 2 * m) * (a + 2 * m + 1))):
            d = 1 + coefficient * d
            c = 1 + coefficient / c
            d = 1 / (d if abs(d) >= tiny else tiny)
            c = c if abs(c) >= tiny else tiny
            change = c * d
            h *= change
        if abs(change - 1) < 1e-14:
            return h
    raise EvalError('the t bound did not converge')


def _beta_cdf(x, a, b):
    if x <= 0:
        return 0.0
    if x >= 1:
        return 1.0
    factor = math.exp(math.lgamma(a + b) - math.lgamma(a) - math.lgamma(b)
                      + a * math.log(x) + b * math.log1p(-x))
    if x < (a + 1) / (a + b + 2):
        return factor * _beta_fraction(a, b, x) / a
    return 1 - factor * _beta_fraction(b, a, 1 - x) / b


@functools.lru_cache(maxsize=512)
def t_quantile(p, df):
    """Student's t quantile by inverting its beta-integral tail. Unlike a truncated normal expansion, this
    remains accurate in the small tails reached by lifetime alpha spending."""
    if not 0 < p < 1 or type(df) is not int or df < 1:
        raise EvalError('a t quantile needs 0 < p < 1 and positive integer degrees of freedom')
    if p == 0.5:
        return 0.0
    tail = min(p, 1 - p)

    def survival(t):
        return 0.5 * _beta_cdf(df / (df + t * t), df / 2, 0.5)
    lo, hi = 0.0, 1.0
    while survival(hi) > tail:
        hi *= 2
    for _ in range(80):
        mid = (lo + hi) / 2
        if survival(mid) > tail:
            lo = mid
        else:
            hi = mid
    return hi if p > 0.5 else -hi


def cluster_bound(deltas, groups, a, side):
    """One-sided (1 - a) bound on the mean of per-case deltas, cluster-robust by group, t with G - 1 df."""
    # ceiling: Cluster-robust inference is approximate; alpha spending cannot supply finite-sample calibration.
    n, mean = len(deltas), sum(deltas) / len(deltas)
    by = {}
    for x, g in zip(deltas, groups):
        by[g] = by.get(g, 0.0) + (x - mean)
    G = len(by)
    if G < 2:
        return None
    se = math.sqrt(G / (G - 1) * sum(s * s for s in by.values())) / n
    q = t_quantile(1 - a, G - 1) * se
    return mean - q if side == 'lower' else mean + q


def bootstrap_bound(deltas, groups, a, side, n_boot, seed):
    """One-sided (1 - a) percentile bound on the mean of per-case deltas, resampling whole groups."""
    # ceiling: A finite bootstrap has Monte Carlo error and cannot resolve tails smaller than about 1 / n_boot.
    by = {}
    for x, g in zip(deltas, groups):
        by.setdefault(g, []).append(x)
    keys, rng, means = sorted(by), random.Random(seed), []
    for _ in range(n_boot):
        pick = [x for _ in keys for x in by[rng.choice(keys)]]
        means.append(sum(pick) / len(pick))
    means.sort()
    k = min(n_boot - 1, max(0, int(a * n_boot) if side == 'lower' else int((1 - a) * n_boot)))
    return means[k]


def score(store, d):
    spec = checked(store, d)
    with settled(store, d, spec):
        return _score(store, d)


def _score(store, d):
    """Per-case means for every grader and arm, paired deltas against each comparator with both bounds, costs and
    failures. Releasing validation or test scores records an exposure."""
    store, spec = Path(store), checked(store, d)
    keyfile = store / 'keys' / f'{d}.json'
    key = json.loads(keyfile.read_text(encoding='utf-8')) if keyfile.exists() else {}
    pool, ids, a = cases(store), split_cases(store, spec['split'], spec), prereg(store, d)['alpha']
    out_dir = store / 'runs' / d / 'out'
    integrity = []
    key_seals = [r for r in ledger(store) if r['kind'] == 'blind_key' and r['digest'] == d]
    if (keyfile.exists() and not key_seals) or (key_seals and (
            not keyfile.exists() or key_seals[-1]['artifact'] != file_sha(keyfile))):
        integrity.append('blind key is unsealed or changed since grading')
    if set(key.values()) != {str(p.parent.relative_to(out_dir)) for p in out_dir.glob('*/*/result.md')}:
        integrity.append('blind key does not cover exactly the outputs')
    slot_seals = outcomes(store, d, 'slot')
    for c, arm, r in slots(store, spec):
        item = f'{c}/{arm}.{r}'
        seal = slot_seals.get(item)
        if not seal or seal.get('artifact') != artifact_hash(out_dir / item) or seal['status'] == 'started':
            integrity.append(f'slot {item}: outcome unsealed, changed or interrupted')
    reviews_of = {}
    for rid, slot in key.items():
        reviews_of.setdefault(slot.split('/')[0], set()).add(rid)
    failed = sorted(str(p.parent.relative_to(out_dir)) for p in out_dir.glob('*/*/failed.json'))
    expected = len(ids) * len(spec['arms']) * spec['runs']
    done = len(list(out_dir.glob('*/*/result.md')))
    report = {'digest': d, 'split': spec['split'], 'alpha': a, 'cases': len(ids),
              'groups': len({pool[c]['group'] for c in ids}), 'expected_slots': expected, 'completed_slots': done,
              'failed': failed, 'integrity_errors': integrity, 'graders': {}, 'cost_per_review': {}, 'cost_unknown': cost(store, d, spec)[1]}
    for arm in spec['arms']:
        costs = [json.loads(p.read_text())['cost_usd'] for p in out_dir.glob(f'*/{arm}.*/summary.json')]
        unpriced = sum(json.loads(p.read_text())['unpriced_calls'] for p in out_dir.glob(f'*/{arm}.*/summary.json'))
        report['cost_per_review'][arm] = (sum(costs) / len(costs)) if costs and not unpriced else None
    for g in spec['graders']:
        gdir = store / 'runs' / d / 'grades' / sha(g)[:12]
        per = {}  # (case, arm) -> [(T, W, U, R) per run]
        missing = []
        for cid in ids:
            path = gdir / f'{cid}.json'
            graded = json.loads(path.read_text(encoding='utf-8')) if path.exists() else None
            seal = outcomes(store, d, 'grade_outcome').get(f'{cid}/{sha(g)}')
            if graded is None or not seal or seal.get('artifact') != file_sha(path) or set(graded) != reviews_of.get(cid, set()):  # every review graded, nothing else
                missing.append(cid)
                continue
            gold_count = len(gold_ids(store, cid))
            for rid, gr in graded.items():
                arm = key[rid].split('/')[1].split('.')[0]
                t, w = len(gr['found']), len(gr['invalid'])
                per.setdefault((cid, arm), []).append((t, w, t - w, t / gold_count if gold_count else 0.0))
        means = {k: tuple(sum(x[i] for x in v) / len(v) for i in range(len(METRICS))) for k, v in per.items()}
        entry = {'missing_grades': missing, 'arms': {}, 'vs': {}}
        for arm in spec['arms']:
            vals = [means[(c, arm)] for c in ids if (c, arm) in means]
            entry['arms'][arm] = {m: (sum(v[i] for v in vals) / len(vals) if vals else None) for i, m in enumerate(METRICS)}
        for comp in spec['comparators']:
            both = [c for c in ids if (c, spec['candidate']) in means and (c, comp) in means]
            groups = [pool[c]['group'] for c in both]
            stats_ = {}
            for i, m in enumerate(METRICS):
                deltas = [means[(c, spec['candidate'])][i] - means[(c, comp)][i] for c in both]
                if not deltas:
                    stats_[m] = None
                    continue
                stats_[m] = {'mean': sum(deltas) / len(deltas),
                             't_lower': cluster_bound(deltas, groups, a, 'lower'),
                             't_upper': cluster_bound(deltas, groups, a, 'upper'),
                             'boot_lower': bootstrap_bound(deltas, groups, a, 'lower', spec['bootstrap'], spec['seed']),
                             'boot_upper': bootstrap_bound(deltas, groups, a, 'upper', spec['bootstrap'], spec['seed'])}
            entry['vs'][comp] = {'cases': len(both), **stats_}
        report['graders'][g] = entry
    (store / 'runs' / d / 'score.json').write_text(json.dumps(report, indent=1), encoding='utf-8')
    if spec['split'] != 'dev' and not any(r['kind'] == 'exposure' and r['digest'] == d for r in ledger(store)):
        record(store, 'exposure', digest=d, split=spec['split'], cases=ids)
    return report


# ---------- decide ----------

def cost_ratio(candidate, comparator):
    if candidate is None or comparator is None:
        return None
    return (1.0 if candidate == 0 else math.inf) if comparator == 0 else candidate / comparator


def gates(spec, report):
    """[(kind, reason)] for every gate the candidate does not pass; kind is 'inconclusive' (the data cannot decide)
    or 'fail'. Empty means every gate passed."""
    why, missing = [], []
    missing.extend(report.get('integrity_errors', []))
    if report['failed'] or report['completed_slots'] != report['expected_slots']:
        missing.append(f"incomplete: {report['completed_slots']}/{report['expected_slots']} slots, {len(report['failed'])} failed")
    if report['groups'] < spec['min_groups']:
        missing.append(f"only {report['groups']} repository groups (need {spec['min_groups']})")
    if spec['budget_usd'] is not None and report.get('cost_unknown'):
        missing.append('under a budget, part of the experiment\'s spending is unknown')
    mu, mw = spec['margins']['U'], spec['margins']['W']
    cost = report['cost_per_review']
    for comp in spec['comparators']:
        c, b = cost.get(spec['candidate']), cost.get(comp)
        limit = spec['max_cost_ratio'] if spec['objective'] == 'quality' else 1 - spec['min_saving']
        ratio = cost_ratio(c, b)
        if ratio is None:
            missing.append(f'cost vs {comp} unknown')
        elif ratio > limit:
            why.append(f'cost vs {comp}: {ratio:.2f}x (limit {limit:.2f}x)')
    for g, entry in report['graders'].items():
        if entry['missing_grades']:
            missing.append(f"grader {g}: missing grades for {len(entry['missing_grades'])} cases")
        for comp, v in entry['vs'].items():
            if not v.get('U') or None in (v['U']['t_lower'], v['W']['t_upper']):
                missing.append(f'grader {g} vs {comp}: not enough data for bounds')
                continue
            need_u = mu if spec['objective'] == 'quality' else -mu
            for method in ('t', 'boot'):
                if not v['U'][f'{method}_lower'] > need_u:
                    why.append(f"grader {g} vs {comp} ({method}): lower bound of U gain {v['U'][f'{method}_lower']:.2f} <= {need_u}")
                if spec['objective'] == 'quality' and not v['R'][f'{method}_lower'] > 0:
                    why.append(f"grader {g} vs {comp} ({method}): lower bound of recall gain {v['R'][f'{method}_lower']:.2f} <= 0")
                if not v['W'][f'{method}_upper'] < mw:
                    why.append(f"grader {g} vs {comp} ({method}): upper bound of extra wrong findings {v['W'][f'{method}_upper']:.2f} >= {mw}")
    return [('inconclusive', m) for m in missing] + [('fail', m) for m in why]


def decide(store, d):
    """Score, apply every gate, and record one decision; a test decision retires its cases in the same record."""
    store, spec = Path(store), checked(store, d)
    with settled(store, d, spec), locked(store):
        rows = ledger(store)
        if any(r['kind'] == 'decision' and r['digest'] == d for r in rows):
            raise EvalError('this experiment is already decided')
        ids = split_cases(store, spec['split'], spec)
        if spec['split'] == 'test' and burned(rows, besides=d) & set(ids):
            raise EvalError('some of these test cases were exposed or retired by another experiment')
        report = _score(store, d)
        found = gates(spec, report)
        result = 'pass' if not found else 'inconclusive' if any(k == 'inconclusive' for k, _ in found) else 'fail'
        why = [m for _, m in found]
        record(store, 'decision', digest=d, split=spec['split'], result=result, reasons=why, alpha=report['alpha'],
               retired=ids if spec['split'] == 'test' else [], allow_tool_backends=spec['allow_tool_backends'])
        return result, why


# ---------- ReviewBench import ----------

def git(repo, *args):
    return subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True).stdout.decode('utf-8')


def component(root, name):
    if not isinstance(name, str) or name in ('', '.', '..') or Path(name).name != name:
        raise EvalError(f'unsafe name in the ReviewBench manifest: {name!r}')
    p = Path(root) / name
    if p.resolve().parent != Path(root).resolve():
        raise EvalError('manifest path escapes its root')
    return p


def import_reviewbench(store, corpus, clones, max_chars=140_000, languages=None):
    """Cases from a ReviewBench checkout: PR text, diff, changed files after the change and the README as the
    material; the human-validated true positives introduced by the PR as the gold, plus a canary line."""
    store, corpus, clones = Path(store), Path(corpus), Path(clones)
    private_store(store)
    pool = cases(store) if (store / 'cases.json').exists() else {}
    canaries = json.loads((store / 'canaries.json').read_text()) if (store / 'canaries.json').exists() else {}
    skipped = []
    for t in json.loads((corpus / 'corpus' / 'manifest.json').read_text(encoding='utf-8')):
        nwo = t['nwo'].replace('/', '_')
        component(clones, nwo)
        cid = f"{nwo}_{int(t['pr_number'])}"
        for field in ('base', 'head'):
            if not re.fullmatch(r'[0-9a-fA-F]+', t[field]):
                raise EvalError('manifest base and head must be hexadecimal object ids')
        if cid in pool or (languages and t['language'] not in languages):
            continue
        key = f"{t['nwo'].replace('/', '_')}_{t['pr_number']}-{t['head'][:8]}"
        golden = json.loads(component(corpus / 'golden', f'{key}.json').read_text(encoding='utf-8'))['findings']
        tps = [x for x in golden if x['tp_fp'] == 'tp' and x.get('scope', 'introduced-by-pr') == 'introduced-by-pr']
        if not tps:
            continue
        if any(any(c in x['file'] for c in '\r\n`') for x in tps):
            skipped.append({'case': cid, 'reason': 'unsupported gold location'})
            continue
        repo = component(clones, nwo)
        if not repo.exists():
            subprocess.run(['git', 'clone', '-q', '--filter=blob:none', '--no-checkout',
                            f"https://github.com/review-bench/{t['nwo'].replace('/', '_')}", str(repo)], check=True)
        if Path(git(repo, 'rev-parse', '--show-toplevel').strip()).resolve() != repo.resolve():
            raise EvalError('the clone folder is not a repository root')
        for rev in (t['base'], t['head']):
            if subprocess.run(['git', '-C', str(repo), 'cat-file', '-e', rev], capture_output=True).returncode:
                git(repo, 'fetch', '-q', 'origin', rev)
        try:
            diff = git(repo, 'diff', t['base'], t['head'])
            files = [f for f in git(repo, 'diff', '--name-only', '-z', '--diff-filter=d', t['base'], t['head']).split('\0') if f]
            parts = [f"=== PR description ===\n# {t['title']}\n\n{t.get('body') or ''}", f'=== change.diff ===\n{diff}']
            head = t['head']
            parts += [f'=== {f} (after the change) ===\n' + git(repo, 'show', f'{head}:{f}') for f in files]
            for readme in ('README.md', 'README.rst', 'README'):
                if readme not in files and not subprocess.run(['git', '-C', str(repo), 'cat-file', '-e', f"{head}:{readme}"],
                                                              capture_output=True).returncode:
                    parts.append(f"=== {readme} ===\n{git(repo, 'show', head + ':' + readme)}")
                    break
        except UnicodeError:
            skipped.append({'case': cid, 'reason': 'non-text material'})
            continue
        material = '\n'.join(parts)
        if '\0' in material:
            skipped.append({'case': cid, 'reason': 'non-text material'})
            continue
        if len(material) > max_chars:
            continue
        canary = secrets.token_hex(16)
        gold = [f"# Ground truth: {t['nwo']}#{t['pr_number']}", f'CANARY {canary}', '']
        gold += [format_gold(i, x) for i, x in enumerate(tps, 1)]
        d = component(store / 'cases', cid)
        d.mkdir(parents=True, exist_ok=True)
        (d / 'material.txt').write_text(material, encoding='utf-8')
        (d / 'gold.md').write_text('\n'.join(gold) + '\n', encoding='utf-8')
        created = git(repo, 'show', '-s', '--format=%cI', t['head']).strip()
        pool[cid] = {'group': t['nwo'], 'source': 'reviewbench', 'language': t['language'], 'created': created,
                     'gold': len(tps)}
        canaries[cid] = canary
    (store / 'cases.json').write_text(json.dumps({'cases': pool}, indent=1), encoding='utf-8')
    (store / 'canaries.json').write_text(json.dumps(canaries, indent=1), encoding='utf-8')
    record(store, 'import', source='reviewbench', cases=len(pool), skipped=skipped)
    return pool


def leaked(store, text):
    """Case ids whose canary appears in text (an improver's inputs or outputs)."""
    path = Path(store) / 'canaries.json'
    canaries = json.loads(path.read_text(encoding='utf-8')) if path.exists() else {}
    return sorted(cid for cid, c in canaries.items() if c in text)


# ---------- CLI ----------

def main(argv=None):
    parser = argparse.ArgumentParser(prog='evaluate.py', description='Evaluate Ringo pipelines with guardrails.')
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('import-reviewbench', help='build cases from a ReviewBench checkout')
    p.add_argument('--corpus', required=True)
    p.add_argument('--clones', required=True)
    p.add_argument('--max-chars', type=int, default=140_000)
    p.add_argument('--language', action='append')
    p = sub.add_parser('trust', help='vouch for the CLI behind a backend (records its path and content hash)')
    p.add_argument('--backend', required=True)
    p.add_argument('--family', required=True, help='e.g. claude, codex, ollama')
    p = sub.add_parser('split', help='assign repository groups to dev, validation and test')
    p.add_argument('--seed', type=int, default=1)
    p.add_argument('--dev', action='extend', nargs='+', default=[], metavar='ID', help='case ids that must be in dev; repeatable')
    p.add_argument('--fractions', default='0.2,0.3,0.5')
    p.add_argument('--resplit', action='store_true')
    p.add_argument('--alpha', type=float, default=POLICY_DEFAULTS['alpha'], help='lifetime error budget (first split only)')
    p.add_argument('--max-validation-submissions', type=int, default=POLICY_DEFAULTS['max_validation_submissions'],
                   help='validation submissions across campaigns sharing validation cases (first split only)')
    p = sub.add_parser('freeze', help='preregister an experiment; prints its digest')
    p.add_argument('--spec', required=True)
    for name in ('run', 'grade', 'score', 'decide', 'void'):
        p = sub.add_parser(name)
        p.add_argument('--digest', required=True)
        if name == 'void':
            p.add_argument('--reason', required=True)
        if name == 'run':
            p.add_argument('--parallel', type=int, default=3)
            p.add_argument('--stop-on-failure', action='store_true')
    for p in sub.choices.values():
        p.add_argument('--store', required=True)
    args = parser.parse_args(argv)
    try:
        outside_repo(args.store, 'store')
        if args.command == 'import-reviewbench':
            print(len(import_reviewbench(args.store, args.corpus, args.clones, args.max_chars, args.language)), 'cases')
        elif args.command == 'trust':
            rec = trust(args.store, args.backend, args.family)
            print(f"trusted {rec['path']} ({rec['sha256'][:12]}) as {rec['family']}")
        elif args.command == 'split':
            doc = split(args.store, args.seed, args.dev, tuple(float(x) for x in args.fractions.split(',')), args.resplit,
                        {'alpha': args.alpha, 'max_validation_submissions': args.max_validation_submissions})
            print(json.dumps({s: len(doc[s]) for s in SPLITS}))
        elif args.command == 'freeze':
            print(freeze(args.store, args.spec))
        elif args.command == 'run':
            failed = run(args.store, args.digest, args.parallel, stop_on_failure=args.stop_on_failure)
            print(f'{len(failed)} failed slots')
        elif args.command == 'grade':
            grade(args.store, args.digest)
        elif args.command == 'void':
            print(void(args.store, args.digest, args.reason))
        elif args.command == 'score':
            print(json.dumps(score(args.store, args.digest), indent=1))
        else:
            result, why = decide(args.store, args.digest)
            print(f'{result} (advisory: a person adopts a change, after a sealed-test pass)')
            for w in why:
                print(f'- {w}')
            return 0 if result == 'pass' else 1
    except (EvalError, ValueError, OSError, subprocess.CalledProcessError) as exc:
        print(f'evaluate: {exc}', file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    sys.exit(main())
