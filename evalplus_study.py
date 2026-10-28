"""AGCL evaluation on HumanEval+ (EvalPlus): the sealed paired-prefix study and the seeded-review study.

Each HumanEval+ task becomes a `work` spec: the writer completes solution.py from its signature and docstring, the
original HumanEval tests are the public check, and EvalPlus's extra inputs (about 80 times as many) are hidden tests
graded locally after the run. Runs use the checkout's workflow.py, the code under evaluation: every run is the full
baseline, and round 1 is eligible to stop when it passes the public check.

  run     prepare and run tasks, J at a time (model calls: at most max_rounds + 1 per task)
  grade   score round 1 and the delivered files against the hidden tests (no model calls)
  seed    plant a defect the public check misses into correct round-1 drafts and give each one review (one call each);
          --reviewer BACKEND has another model review the same seeded drafts, and the drafts before seeding (two
          calls each), so two reviewers' catch rates and false alarms compare on identical material
  draft   one round-1 draft per task by another tool-free writer (--writer BACKEND; one call each), for the
          probe's cross-family pair
  probe   distinguishing inputs, replayed: run each pair of implementations the study has (round 1 and the delivered
          files; a correct draft and its seeded defect) on inputs mutated from the public tests (no model calls)
  report  shadow-label bounds for the runs, the seeded reviews' catch rate and the probe's (no model calls)

Run folders and the dataset cache go under --out, outside this checkout. HumanEval+ is downloaded at a pinned
version and checksum; EvalPlus is Apache-2.0 (https://github.com/evalplus/evalplus), and the output comparison
follows its evaluator: exact equality, a float tolerance, and a root check for find_zero.

usage: python3 evalplus_study.py {run,grade,seed,draft,probe,report} --out DIR [--limit N] [--jobs J]
"""
import argparse
import concurrent.futures
import contextlib
import copy
import gzip
import hashlib
import io
import json
import math
import os
import random
import subprocess
import sys
import tempfile
import tokenize
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
DATA_URL = 'https://github.com/evalplus/humanevalplus_release/releases/download/v0.1.10/HumanEvalPlus.jsonl.gz'
DATA_SHA256 = '272720b90ac375502c8ed23cd791c2a93dfb22a911641a494da74a426c09f101'
GRADE_TIMEOUT_S = 120
# Hidden tests sit on disk beside the specs, so the writer and reviewer must be tool-free: they cannot read files.
TOOL_FREE = ('codex-chatgpt', 'claude')
# HumanEval/32 (find_zero): the dataset's own reference fails EvalPlus's root check on 6 of its 888 inputs, so no
# floating-point answer can be relied on to pass; it would grade noise.
EXCLUDED = {'HumanEval/32'}


def dataset(out):
    path = out / 'data' / 'HumanEvalPlus-v0.1.10.jsonl.gz'
    if not path.is_file():
        path.parent.mkdir(parents=True, exist_ok=True)
        with urllib.request.urlopen(DATA_URL, timeout=120) as response:
            data = response.read()
        if hashlib.sha256(data).hexdigest() != DATA_SHA256:
            raise ValueError('HumanEval+ download does not match its pinned checksum')
        path.write_bytes(data)
    elif hashlib.sha256(path.read_bytes()).hexdigest() != DATA_SHA256:
        raise ValueError(f'{path} does not match the pinned checksum; delete it to download again')
    with gzip.open(path, 'rt', encoding='utf-8') as f:
        return {t['task_id']: t for t in map(json.loads, f)}


def slug(task_id):
    return task_id.replace('/', '-')


def hidden(out, task_id):
    """The task's EvalPlus record (hidden inputs and reference), one file per task so a grade loads only its own."""
    return out / 'tasks' / slug(task_id) / 'hidden.json'


def task_spec(task, template, backend=None):
    spec = json.loads(Path(template).read_text(encoding='utf-8'))
    # The star import brings helpers the prompt defines and the test calls (HumanEval/38 and /50 call encode_*).
    check = (f"from solution import *\nfrom solution import {task['entry_point']}\n{task['test']}\n\n"
             f"check({task['entry_point']})\n"
             "print('base tests pass')\n")
    spec.update(brief=f"Complete `{task['entry_point']}` in solution.py. Its signature and docstring are the "
                      'specification. Keep the signature, use only the standard library, and do not print.\n',
                files={'solution.py': task['prompt']}, fixtures={'check_base.py': check},
                checks=[{'name': 'base tests', 'argv': ['{python}', '-B', 'check_base.py'], 'timeout_s': 60}])
    if backend:
        spec.update(writer=backend, judge=backend)
    for name in (spec['writer'], spec['judge']):
        if name not in TOOL_FREE and not name.startswith('ollama:'):
            raise ValueError(f'{name} may read files; the study needs a tool-free backend ({", ".join(TOOL_FREE)}, ollama:MODEL)')
    if backend:
        if not backend.startswith('codex'):
            spec.pop('roles', None)  # a Claude backend takes a model, not an effort
    return spec


# --- hidden-test grading (runs in a child interpreter: model-written code) ---

def is_floats(x):
    return isinstance(x, float) or (isinstance(x, (list, tuple)) and bool(x) and all(isinstance(i, float) for i in x))


def same(out, exp, atol, entry_point, inp):
    """EvalPlus's HumanEval comparison: exact, else within atol (1e-6 for float results) at rtol 1e-7."""
    if entry_point == 'find_zero':
        xs = inp[0]
        return abs(sum(c * math.pow(out, i) for i, c in enumerate(xs))) <= atol
    if out == exp:
        return True
    if atol == 0 and is_floats(exp):
        atol = 1e-6
    if atol == 0 or type(out) is not type(exp):
        return False
    pairs = list(zip(out, exp)) if isinstance(exp, (list, tuple)) else [(out, exp)]
    if isinstance(exp, (list, tuple)) and len(out) != len(exp):
        return False
    try:
        return all(abs(o - e) <= atol + 1e-7 * abs(e) for o, e in pairs)
    except TypeError:
        return False


def grade_here(task, source):
    """True when source passes every base and plus input. Runs inside the grading child."""
    reference, candidate = {}, {}
    exec(task['prompt'] + task['canonical_solution'], reference)
    with open(os.devnull, 'w') as quiet, contextlib.redirect_stdout(quiet):
        return _grade_quiet(task, source, reference, candidate)


def _grade_quiet(task, source, reference, candidate):
    exec(source, candidate)
    ref, fn = reference[task['entry_point']], candidate.get(task['entry_point'])
    if fn is None:
        return False
    atol = float(task.get('atol') or 0)
    for inp in task['base_input'] + task['plus_input']:
        expected = ref(*copy.deepcopy(inp))
        try:
            if not same(fn(*copy.deepcopy(inp)), expected, atol, task['entry_point'], inp):
                return False
        except BaseException:
            return False
    return True


def grade(out, task_id, path):
    """Whether the file at path passes the hidden tests, judged in an isolated child with a time limit."""
    if not path or not Path(path).is_file():
        return False
    try:
        proc = subprocess.run([sys.executable, '-I', '-B', str(Path(__file__).resolve()), '_grade', '--out', str(out),
                               '--task', str(hidden(out, task_id)), '--file', str(path)], capture_output=True, text=True,
                              timeout=GRADE_TIMEOUT_S, cwd=tempfile.gettempdir())
    except subprocess.TimeoutExpired:
        return False
    return proc.stdout.strip().endswith('PASS')


# HumanEval solutions never need files or processes. Draft code runs with the user's permissions, so it could read the
# hidden tests beside the specs; a draft that names any of these is flagged and left out of the bounds.
FILE_ACCESS = {'open', 'os', 'pathlib', 'glob', 'io', 'subprocess', 'shutil', 'importlib', '__import__', '__file__',
               'builtins', 'socket', 'urllib'}


def touches_files(source):
    try:
        return any(t.type == tokenize.NAME and t.string in FILE_ACCESS
                   for t in tokenize.generate_tokens(io.StringIO(source).readline))
    except (tokenize.TokenError, SyntaxError, IndentationError):
        return True  # unreadable: do not trust it


def delivered(run):
    """The files the full baseline delivered: final/, else the last round whose candidate passed every check."""
    if (run / 'final').is_dir():
        return run / 'final'
    report = json.loads((run / 'report.json').read_text(encoding='utf-8'))
    for entry in reversed(report.get('rounds') or []):
        folder = run / f"round-{entry['round']}" / 'candidate'
        if entry.get('checks') and all(c['status'] == 'pass' for c in entry['checks']) and folder.is_dir():
            return folder
    return None


# --- seeded defects ---

SWAP = {'<': '<=', '<=': '<', '>': '>=', '>=': '>', '==': '!=', '!=': '==', '+': '-', '-': '+', '*': '+',
        'and': 'or', 'or': 'and', 'min': 'max', 'max': 'min', 'True': 'False', 'False': 'True', 'any': 'all',
        'all': 'any'}


def mutants(source):
    """Every single-token mutation of the code (not strings, comments or def/import lines), formatting kept."""
    lines = source.splitlines(True)
    try:
        tokens = list(tokenize.generate_tokens(io.StringIO(source).readline))
    except (tokenize.TokenError, SyntaxError):
        return
    for tok in tokens:
        text, (row, col), (erow, ecol) = tok.string, tok.start, tok.end
        if tok.type not in (tokenize.NUMBER, tokenize.OP, tokenize.NAME) or row != erow or row > len(lines):
            continue
        line = lines[row - 1]
        if line.lstrip().startswith(('def ', 'from ', 'import ', '@')):
            continue
        if tok.type == tokenize.NUMBER and text.isdigit():
            new = str(int(text) + 1)
        elif text in SWAP and tok.type in (tokenize.OP, tokenize.NAME):
            new = SWAP[text]
        else:
            continue
        mutated = ''.join(lines[:row - 1]) + line[:col] + new + line[ecol:] + ''.join(lines[row:])
        try:
            compile(mutated, 'mutant', 'exec')
        except SyntaxError:
            continue
        yield f'line {row}: {text} -> {new}', mutated


def base_passes(spec, source):
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / 'solution.py').write_text(source, encoding='utf-8')
        for name, text in spec['fixtures'].items():
            (root / name).write_text(text, encoding='utf-8')
        try:
            proc = subprocess.run([sys.executable, '-B', 'check_base.py'], cwd=root, capture_output=True, timeout=60)
        except subprocess.TimeoutExpired:
            return False
        return proc.returncode == 0


# --- commands ---

def tasks_for(out, limit, template, backend):
    data = dataset(out)
    ids = [t for t in data if t not in EXCLUDED]
    ids = ids[:limit] if limit else ids
    for task_id in ids:
        folder = out / 'tasks' / slug(task_id)
        if not (folder / 'task.json').is_file():
            folder.mkdir(parents=True, exist_ok=True)
            hidden(out, task_id).write_text(json.dumps(data[task_id]), encoding='utf-8')
            (folder / 'task.json').write_text(json.dumps(task_spec(data[task_id], template, backend), indent=1),
                                              encoding='utf-8')
    return ids


RETRY = (None, 'error', 'interrupted', 'budget_exhausted')  # outcomes of the run, not of the task


def pending(runs, ids):
    """Tasks still to run. A run with no report, or one that ended in an error, an interruption or a spent budget
    (a usage limit, a lost login), is set aside as .incomplete-N and run again; other outcomes are results."""
    todo = []
    for t in ids:
        folder = runs / slug(t)
        status = json.loads((folder / 'report.json').read_text(encoding='utf-8')).get('status') \
            if (folder / 'report.json').is_file() else None
        if status in RETRY:
            if folder.exists():
                folder.rename(runs / f'{slug(t)}.incomplete-{sum(1 for _ in runs.glob(slug(t) + ".incomplete-*"))}')
            todo.append(t)
    return todo


def run_all(out, ids, jobs):
    runs = out / 'runs'
    runs.mkdir(exist_ok=True)
    todo = pending(runs, ids)
    print(f'{len(todo)} tasks to run, {jobs} at a time; each makes at most max_rounds + 1 model calls', flush=True)
    failures = 0

    def one(task_id):
        proc = subprocess.run([sys.executable, str(HERE / 'workflow.py'), '--spec',
                               str(out / 'tasks' / slug(task_id) / 'task.json'), '--out', str(runs / slug(task_id))],
                              capture_output=True, text=True)
        return task_id, proc
    with concurrent.futures.ThreadPoolExecutor(jobs) as pool:
        active = {pool.submit(one, t) for t in todo[:jobs]}
        queue = todo[jobs:]
        while active:
            done, active = concurrent.futures.wait(active, return_when=concurrent.futures.FIRST_COMPLETED)
            for future in done:
                task_id, proc = future.result()
                status = (json.loads(proc.stdout or '{}') if proc.stdout.strip().startswith('{') else {}).get('status')
                print(f'{task_id}: {status or "failed"} {proc.stderr.strip()[-200:] if not status else ""}', flush=True)
                # Three runs in a row that end in an error (a usage limit, a lost login) stop new launches.
                failures = failures + 1 if status in RETRY else 0
                if queue and failures < 3:
                    active.add(pool.submit(one, queue.pop(0)))
            if failures >= 3:
                print('stopping: three runs in a row failed; fix the cause and rerun to continue', flush=True)
                queue.clear()


def grade_all(out):
    count = 0
    for run in sorted((out / 'runs').glob('HumanEval-*')):
        if not (run / 'report.json').is_file() or '.incomplete-' in run.name:
            continue
        task_id = run.name.replace('HumanEval-', 'HumanEval/')
        drafts = [p for p in run.glob('round-*/*/solution.py')] + [run / 'final/solution.py']
        suspect = any(touches_files(p.read_text(encoding='utf-8', errors='replace')) for p in drafts if p.is_file())
        result = {'round1_pass': grade(out, task_id, run / 'round-1/candidate/solution.py'),
                  'delivered_pass': grade(out, task_id, (delivered(run) or run / 'missing') / 'solution.py'),
                  'suspect': suspect}
        (run / 'grade.json').write_text(json.dumps(result) + '\n', encoding='utf-8')
        count += 1
    print(f'graded {count} runs')


def seed_all(out, limit, jobs, reviewer=None):
    import workflow
    data, chosen = dataset(out), []
    for run in sorted((out / 'runs').glob('HumanEval-*')):
        if limit and len(chosen) >= limit:
            break
        if '.incomplete-' in run.name or not (run / 'grade.json').is_file():
            continue
        grade_result = json.loads((run / 'grade.json').read_text())
        report = json.loads((run / 'report.json').read_text(encoding='utf-8'))
        first = (report.get('rounds') or [{}])[0].get('checks')
        if not first or any(c['status'] != 'pass' for c in first) or not grade_result['round1_pass'] \
                or grade_result.get('suspect'):
            continue  # only correct, eligible drafts: the planted defect is then the only defect
        if (out / 'seeded' / run.name / 'solution.py').is_file():  # seeded already: reuse the same draft
            chosen.append(run.name)
            continue
        task_id = run.name.replace('HumanEval-', 'HumanEval/')
        spec = workflow.load_spec(out / 'tasks' / run.name / 'task.json')
        source = (run / 'round-1/candidate/solution.py').read_text(encoding='utf-8')
        for where, mutated in list(mutants(source))[:80]:
            if base_passes(spec, mutated) and not grade(out, task_id, write_temp(out, mutated)):
                folder = out / 'seeded' / run.name
                folder.mkdir(parents=True, exist_ok=True)
                (folder / 'solution.py').write_text(mutated, encoding='utf-8')
                (folder / 'mutation.txt').write_text(where + '\n', encoding='utf-8')
                chosen.append(run.name)
                break
    # The template's judge already reviewed each clean draft during its run; another reviewer reviews it here too.
    todo = [(name, clean) for name in chosen for clean in ((False, True) if reviewer else (False,))
            if not (reviewer_folder(out, name, reviewer) / ('clean.json' if clean else 'result.json')).is_file()]
    print(f'{len(chosen)} seeded drafts, {len(todo)} reviews to run: one model call each', flush=True)

    def one(item):
        name, clean = item
        return subprocess.run([sys.executable, str(Path(__file__).resolve()), '_review', '--out', str(out),
                               '--name', name] + (['--reviewer', reviewer] if reviewer else [])
                              + (['--clean'] if clean else []), capture_output=True, text=True)
    with concurrent.futures.ThreadPoolExecutor(jobs) as pool:
        for (name, clean), proc in zip(todo, pool.map(one, todo)):
            print(f'{name}{" (clean)" if clean else ""}: {proc.stdout.strip() or proc.stderr.strip()[-200:]}', flush=True)


def write_temp(out, text):
    folder = out / 'scratch'
    folder.mkdir(exist_ok=True)
    path = folder / (hashlib.sha256(text.encode()).hexdigest()[:16] + '.py')
    path.write_text(text, encoding='utf-8')
    return path


def reviewer_folder(out, name, reviewer):
    """Where a review of a seeded draft goes: the draft's folder for the template's judge, else a subfolder per
    reviewer backend, so one set of seeded drafts can be reviewed by several models."""
    folder = out / 'seeded' / name
    return folder / ''.join(c if c.isalnum() else '-' for c in reviewer) if reviewer else folder


def review_one(out, name, reviewer=None, clean=False):
    """The work loop's own review of a seeded draft: the same material, plan and review role as `work`. With a
    reviewer backend, that backend reviews instead of the template's judge; clean reviews the draft before seeding,
    which measures the reviewer's false alarms (the template's judge already reviewed it during the run)."""
    import harness
    import pipelines
    import workflow
    spec = workflow.load_spec(out / 'tasks' / name / 'task.json')
    name_ = reviewer or spec['judge']
    if name_ not in TOOL_FREE and not name_.startswith('ollama:'):
        raise ValueError(f'{name_} may read files; the study needs a tool-free reviewer ({", ".join(TOOL_FREE)})')
    folder, run = out / 'seeded' / name, out / 'runs' / name
    dest = reviewer_folder(out, name, reviewer)
    dest.mkdir(parents=True, exist_ok=True)
    draft = run / 'round-1/candidate/solution.py' if clean else folder / 'solution.py'
    files = {'solution.py': draft.read_text(encoding='utf-8')}
    rationale = json.loads((run / 'round-1/proposal.json').read_text(encoding='utf-8'))['rationale']
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        workflow.write_files(root, {**files, **spec['fixtures']})
        observed = workflow.checks(spec, root, root, lambda: 600)
    material = workflow.review_material(spec, files, rationale, observed)
    kind = 'clean' if clean else 'seeded'
    trace = harness.outside_repo(dest / ('trace-clean' if clean else 'trace'), f'{kind} review')
    accounting = harness.Run(trace)
    # the template's review role (model, effort) applies to its own kind of backend only
    role = spec['roles'].get('review') if name_.startswith('codex') == spec['judge'].startswith('codex') else None
    backend = workflow.role_backend(harness.backend(name_), role)
    text, _ = harness.run_plan(pipelines.load_plan(None, 'review'), material, backend, accounting,
                               timeout_s=spec['timeout_s'])
    (dest / ('clean-review.md' if clean else 'review.md')).write_text(text + '\n', encoding='utf-8')
    report = json.loads((run / 'report.json').read_text(encoding='utf-8'))
    changed = [line.strip() for line in (folder / 'solution.py').read_text(encoding='utf-8').splitlines()
               if line.strip() and line not in (run / 'round-1/candidate/solution.py').read_text().splitlines()]
    result = {'reviewer': name_, 'findings': workflow.review_findings(text),
              'mentions_change': not clean and any(line in text for line in changed), 'usage': accounting.summary()}
    if not reviewer:
        result['original_findings'] = report['rounds'][0].get('review_findings')
    (dest / ('clean.json' if clean else 'result.json')).write_text(json.dumps(result, indent=1) + '\n', encoding='utf-8')
    return result


# --- distinguishing inputs, replayed offline (no model calls) ---
# Two implementations of one function are run on inputs mutated from the PUBLIC test inputs (EvalPlus's base_input,
# the inputs of the original HumanEval tests), never from the hidden ones; an input where they disagree is a
# distinguishing input. The replay asks whether such locally generated inputs expose the differences the hidden
# tests expose, which decides whether a zero-token probe could stand in for part of a review (docs/AGCL.md).

PROBE_INPUTS = 200  # local CPU only: a few seconds per pair
PROBE_TIMEOUT_S = 60


def mutate(x, rng):
    """One type-aware mutation of a value, in the spirit of EvalPlus's input mutation, kept small enough to run."""
    if isinstance(x, bool):
        return not x
    if isinstance(x, int):
        return rng.choice([x + 1, x - 1, -x, 0, x * 2 if abs(x) < 1000 else x // 2, rng.randint(-20, 20)])
    if isinstance(x, float):
        return rng.choice([x + 1.0, x - 1.0, -x, 0.0, x * 2, x / 2, round(rng.uniform(-20, 20), 3)])
    if isinstance(x, str):
        i = rng.randrange(len(x) + 1)
        pick = rng.choice(x) if x else 'a'
        return rng.choice([x[:i] + x[i + 1:], x[:i] + pick + x[i:], x[:i] + rng.choice('aZ9 _-.') + x[i:],
                           x.swapcase(), x + x[:i], ''])[:1000]
    if isinstance(x, list):
        if not x:
            return [rng.choice([0, 1, 'a', 1.0])]
        i = rng.randrange(len(x))
        return rng.choice([x[:i] + x[i + 1:], x[:i] + [x[i]] + x[i:], x[:i] + [mutate(x[i], rng)] + x[i + 1:],
                           x[::-1], []])[:1000]
    if isinstance(x, tuple):
        return tuple(mutate(list(x), rng)) if not x else x[:(i := rng.randrange(len(x)))] + (mutate(x[i], rng),) + x[i + 1:]
    if isinstance(x, dict) and x:
        key = rng.choice(list(x))
        return {**x, key: mutate(x[key], rng)} if rng.random() < .7 else {k: v for k, v in x.items() if k != key}
    return x


def probe_inputs(base, n=PROBE_INPUTS, seed=0):
    """n distinct new argument lists, each one mutation of a public input or of an earlier mutant (seeded, so a
    replay is repeatable)."""
    rng, pool, found = random.Random(seed), [list(b) for b in base], []
    seen = {repr(list(b)) for b in base}  # the public tests already ran these
    for _ in range(n * 30):
        if len(found) >= n or not pool:
            break
        args = copy.deepcopy(rng.choice(pool))
        if not args:
            continue
        i = rng.randrange(len(args))
        args[i] = mutate(args[i], rng)
        key = repr(args)
        if key not in seen and len(key) < 20000:
            seen.add(key)
            found.append(args)
            pool.append(args)
    return found


def compare_here(entry_point, atol, first, second, inputs):
    """Indexes of the inputs where the two implementations differ. Runs inside the probing child."""
    fns = []
    with open(os.devnull, 'w') as quiet, contextlib.redirect_stdout(quiet):
        for source in (first, second):
            namespace = {}
            exec(source, namespace)
            fns.append(namespace.get(entry_point))
        differ = []
        for i, args in enumerate(inputs):
            outs = []
            for fn in fns:
                try:
                    outs.append(('ok', fn(*copy.deepcopy(args))))
                except BaseException as exc:  # an exception is an outcome; its type is compared
                    outs.append(('raised', type(exc).__name__))
            (a_kind, a), (b_kind, b) = outs
            if a_kind != b_kind or (a_kind == 'raised' and a != b) or (a_kind == 'ok' and not same(a, b, atol, entry_point, args)):
                differ.append(i)
    return differ


def probe(out, task, first, second):
    """{'inputs': n, 'differ': k} for two source files, or {'inconclusive': reason}. The child gets the generated
    inputs and the two files, nothing else."""
    inputs = probe_inputs(task['base_input'])
    with tempfile.TemporaryDirectory() as tmp:
        spec = Path(tmp) / 'probe.json'
        spec.write_text(json.dumps({'entry_point': task['entry_point'], 'atol': float(task.get('atol') or 0),
                                    'first': str(first), 'second': str(second), 'inputs': inputs}), encoding='utf-8')
        try:
            proc = subprocess.run([sys.executable, '-I', '-B', str(Path(__file__).resolve()), '_probe', '--out', str(out),
                                   '--task', str(spec)], capture_output=True, text=True, timeout=PROBE_TIMEOUT_S, cwd=tmp)
        except subprocess.TimeoutExpired:
            return {'inconclusive': 'timeout'}
    lines = proc.stdout.strip().splitlines()
    if proc.returncode or not lines or not lines[-1].startswith('DIFFER '):
        return {'inconclusive': (proc.stderr.strip().splitlines() or ['no result'])[-1][:200]}
    return {'inputs': len(inputs), 'differ': len(json.loads(lines[-1][7:]))}


def backend_folder(name):
    return ''.join(c if c.isalnum() else '-' for c in name)


def draft_one(out, name, writer):
    """One round-1 draft by another writer backend: the same writer prompt and material as the run's round 1, one
    call, no review. A second implementation from a different model family, for the probe's cross-family pair."""
    import harness
    import workflow
    if writer not in TOOL_FREE and not writer.startswith('ollama:'):
        raise ValueError(f'{writer} may read files; the study needs a tool-free writer ({", ".join(TOOL_FREE)})')
    spec = workflow.load_spec(out / 'tasks' / name / 'task.json')
    dest = out / 'drafts' / backend_folder(writer) / name
    dest.mkdir(parents=True, exist_ok=True)
    system = (HERE / 'prompts' / 'work-code.md').read_text(encoding='utf-8')
    role = spec['roles'].get('writer') if writer.startswith('codex') == spec['writer'].startswith('codex') else None
    backend = workflow.role_backend(harness.backend(writer), role)
    feedback, result = {}, {'writer': writer, 'usage': []}
    for attempt in ('draft', 'repair'):  # protocol_repair, as in work: one repair call with the exact error
        reply, usage = harness.call(backend, system, workflow.text_message(spec, spec['files'], feedback),
                                    spec['timeout_s'])
        (dest / f'{attempt}-reply.txt').write_text(reply, encoding='utf-8')
        result['usage'].append(usage)
        try:
            files = workflow.materialize_text(spec['files'], spec['files'], workflow.parse_text_reply(reply))
        except ValueError as exc:
            result.update(base_pass=False, error=str(exc)[:300])
            if not spec.get('protocol_repair'):
                break
            feedback = {'protocol_error': str(exc) + workflow.REPAIR}
            continue
        (dest / 'solution.py').write_text(files['solution.py'], encoding='utf-8')
        result.pop('error', None)
        result.update(base_pass=base_passes(spec, files['solution.py']), repaired=attempt == 'repair')
        break
    (dest / 'result.json').write_text(json.dumps(result, indent=1) + '\n', encoding='utf-8')
    return result


def draft_all(out, writer, jobs):
    names = [r.name for r in sorted((out / 'runs').glob('HumanEval-*'))
             if '.incomplete-' not in r.name and (r / 'grade.json').is_file()]
    todo = [n for n in names if not (out / 'drafts' / backend_folder(writer) / n / 'result.json').is_file()]
    print(f'{len(todo)} drafts to write with {writer}: one model call each', flush=True)

    def one(name):
        return subprocess.run([sys.executable, str(Path(__file__).resolve()), '_draft', '--out', str(out),
                               '--name', name, '--writer', writer], capture_output=True, text=True)
    with concurrent.futures.ThreadPoolExecutor(jobs) as pool:
        for name, proc in zip(todo, pool.map(one, todo)):
            print(f'{name}: {proc.stdout.strip() or proc.stderr.strip()[-200:]}', flush=True)


def probe_all(out):
    """Replay the probe on every pair of implementations the study already has: a run's round 1 against the files
    it delivered when they differ, and each seeded draft against its correct original. No model calls."""
    count = 0
    for run in sorted((out / 'runs').glob('HumanEval-*')):
        if '.incomplete-' in run.name or not (run / 'grade.json').is_file():
            continue
        task = json.loads(hidden(out, run.name.replace('HumanEval-', 'HumanEval/')).read_text(encoding='utf-8'))
        first, final = run / 'round-1/candidate/solution.py', (delivered(run) or run / 'missing') / 'solution.py'
        pairs = {}
        if first.is_file() and final.is_file() and first.read_bytes() != final.read_bytes():
            pairs['revision'] = probe(out, task, first, final)
        seeded = out / 'seeded' / run.name / 'solution.py'
        if first.is_file() and seeded.is_file():
            pairs['seeded'] = probe(out, task, first, seeded)
        for other in sorted((out / 'drafts').glob(f'*/{run.name}/solution.py')) if (out / 'drafts').is_dir() else []:
            if first.is_file():  # a cross-family pair: round 1 against another model's draft of the same task
                pairs[f'cross-{other.parent.parent.name}'] = {
                    **probe(out, task, first, other),
                    'other_pass': grade(out, task['task_id'], other)}
        if pairs:
            (run / 'probe.json').write_text(json.dumps(pairs) + '\n', encoding='utf-8')
            count += 1
    print(f'probed {count} runs')


def report_all(out, alpha):
    import shadow_labels
    rows = [shadow_labels.row(r) for r in shadow_labels.runs([out / 'runs']) if '.incomplete-' not in str(r)]
    if rows:
        print(shadow_labels.summary(rows, alpha))
    probes = []
    for run in sorted((out / 'runs').glob('HumanEval-*')):
        if (run / 'probe.json').is_file() and (run / 'grade.json').is_file():
            probes.append((json.loads((run / 'probe.json').read_text()), json.loads((run / 'grade.json').read_text())))
    revisions = [(p['revision'], g) for p, g in probes if 'revision' in p]
    if revisions:
        told = [p for p, g in revisions if g['round1_pass'] != g['delivered_pass']]  # the hidden tests tell them apart
        alike = [p for p, g in revisions if g['round1_pass'] and g['delivered_pass']]
        hit = lambda ps: f"{sum(p.get('differ', 0) > 0 for p in ps)}/{sum('inconclusive' not in p for p in ps)}"
        print(f'probe, revised runs: local inputs told round 1 from the delivered files apart on {hit(told)} of the '
              f'revisions the hidden tests tell apart, and on {hit(alike)} where both pass the hidden tests; '
              f"{sum('inconclusive' in p for p, _ in revisions)} inconclusive")
    seeded_probes = [p['seeded'] for p, _ in probes if 'seeded' in p]
    if seeded_probes:
        print(f'probe, seeded drafts: local inputs told the planted defect from the correct draft apart on '
              f"{sum(p.get('differ', 0) > 0 for p in seeded_probes)}/{sum('inconclusive' not in p for p in seeded_probes)} "
              f"({sum('inconclusive' in p for p in seeded_probes)} inconclusive; no model calls)")
    for key in sorted({k for p, _ in probes for k in p if k.startswith('cross-')}):
        pairs = [(p[key], g) for p, g in probes if key in p and 'inconclusive' not in p[key]]
        flagged = lambda ps: sum(p['differ'] > 0 for p, _ in ps)
        wrong = [(p, g) for p, g in pairs if not g['round1_pass']]
        right = [(p, g) for p, g in pairs if g['round1_pass']]
        both = sum(g['round1_pass'] or p['other_pass'] for p, g in pairs)
        print(f'probe, {key[6:]} draft against round 1: they disagree on local inputs in {flagged(pairs)}/{len(pairs)} '
              f'tasks; flagged {flagged(wrong)}/{len(wrong)} where round 1 fails the hidden tests and {flagged(right)}/'
              f'{len(right)} where it passes; either draft passes the hidden tests on {both}/{len(pairs)}, round 1 '
              f'alone on {len(right)}/{len(pairs)}; {sum("inconclusive" in p[key] for p, _ in probes if key in p)} inconclusive')
    seeded = out / 'seeded'
    folders = sorted({p.parent for p in seeded.glob('**/result.json')}) if seeded.is_dir() else []
    by_reviewer = {}
    for folder in folders:
        result = json.loads((folder / 'result.json').read_text())
        clean = json.loads((folder / 'clean.json').read_text()) if (folder / 'clean.json').is_file() else None
        alarm = result.get('original_findings') if clean is None else clean['findings']
        default = folder.parent == seeded
        by_reviewer.setdefault('template judge' if default else result['reviewer'], []).append((result, alarm))
    for reviewer, pairs in by_reviewer.items():
        n = len(pairs)
        caught = sum(r['findings'] > 0 for r, _ in pairs)
        located = sum(r['findings'] > 0 and r['mentions_change'] for r, _ in pairs)
        paired = [a for _, a in pairs if a is not None]
        tokens = sum(r['usage'].get(k) or 0 for r, _ in pairs for k in ('input', 'cache_read', 'cache_write', 'output'))
        print(f'seeded reviews ({reviewer}): {caught}/{n} raised a finding on the planted defect '
              f'(lower bound {1 - shadow_labels.upper_bound(n - caught, n, alpha):.1%}); '
              f'{located}/{n} quoted the changed line; {tokens:,} tokens')
        print(f'  same drafts before seeding: {sum(a > 0 for a in paired)}/{len(paired)} reviews raised a finding '
              '(false alarms)')


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('command', choices=('run', 'grade', 'seed', 'draft', 'probe', 'report', '_grade', '_review',
                                            '_probe', '_draft'))
    parser.add_argument('--out', type=Path, required=True, help='study folder outside this checkout')
    parser.add_argument('--limit', type=int, help='first N tasks (run) or seeded drafts (seed)')
    parser.add_argument('--jobs', type=int, default=4, help='parallel runs; Codex usage limits bind near 6')
    parser.add_argument('--template', type=Path, default=HERE / 'examples' / 'v3-codex.json')
    parser.add_argument('--backend', help='writer and judge backend instead of the template\'s')
    parser.add_argument('--alpha', type=float, default=0.05)
    parser.add_argument('--reviewer', help='seed: review the seeded drafts, and the same drafts before seeding, with '
                        'this tool-free backend instead of the template\'s judge')
    parser.add_argument('--writer', help='draft: the other writer backend (tool-free), one call per task')
    parser.add_argument('--task'), parser.add_argument('--file'), parser.add_argument('--name')
    parser.add_argument('--clean', action='store_true')
    args = parser.parse_args(argv)
    out = args.out.expanduser().resolve()
    if args.command == '_grade':
        task = json.loads(Path(args.task).read_text(encoding='utf-8'))
        print('PASS' if grade_here(task, Path(args.file).read_text(encoding='utf-8')) else 'FAIL')
        return 0
    if args.command == '_draft':
        result = draft_one(out, args.name, args.writer)
        print(f"base tests pass: {result['base_pass']}{' (' + result['error'] + ')' if 'error' in result else ''}")
        return 0
    if args.command == '_probe':
        job = json.loads(Path(args.task).read_text(encoding='utf-8'))
        first, second = (Path(job[k]).read_text(encoding='utf-8') for k in ('first', 'second'))
        print('DIFFER ' + json.dumps(compare_here(job['entry_point'], job['atol'], first, second, job['inputs'])))
        return 0
    if args.command == '_review':
        result = review_one(out, args.name, args.reviewer, args.clean)
        print(f"findings {result['findings']}, quoted the change: {result['mentions_change']}")
        return 0
    if out == HERE or HERE in out.parents:
        print('evalplus study: --out must be outside the Ringo checkout', file=sys.stderr)
        return 2
    out.mkdir(parents=True, exist_ok=True)
    out.chmod(0o700)
    if args.command == 'run':
        run_all(out, tasks_for(out, args.limit, args.template, args.backend), max(1, args.jobs))
    elif args.command == 'grade':
        grade_all(out)
    elif args.command == 'seed':
        seed_all(out, args.limit, max(1, args.jobs), args.reviewer)
    elif args.command == 'draft':
        if not args.writer:
            parser.error('draft needs --writer BACKEND')
        draft_all(out, args.writer, max(1, args.jobs))
    elif args.command == 'probe':
        probe_all(out)
    else:
        report_all(out, args.alpha)
    return 0


if __name__ == '__main__':
    sys.exit(main())
