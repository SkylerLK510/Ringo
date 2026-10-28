"""AGCL evaluation on HumanEval+ (EvalPlus): the sealed paired-prefix study and the seeded-review study.

Each HumanEval+ task becomes a `work` spec: the writer completes solution.py from its signature and docstring, the
original HumanEval tests are the public check, and EvalPlus's extra inputs (about 80 times as many) are hidden tests
graded locally after the run. Runs use the checkout's workflow.py, the code under evaluation, in shadow mode
(stop_on_checks retain 1): every run is the full baseline and also records whether round 1 was eligible to stop.

  run     prepare and run tasks, J at a time (model calls: at most max_rounds + 1 per task)
  grade   score round 1 and the delivered files against the hidden tests (no model calls)
  seed    plant a defect the public check misses into correct round-1 drafts and give each one review (one call each)
  report  shadow-label bounds for the runs, and the seeded reviews' catch rate (no model calls)

Run folders and the dataset cache go under --out, outside this checkout. HumanEval+ is downloaded at a pinned
version and checksum; EvalPlus is Apache-2.0 (https://github.com/evalplus/evalplus), and the output comparison
follows its evaluator: exact equality, a float tolerance, and a root check for find_zero.

usage: python3 evalplus_study.py {run,grade,seed,report} --out DIR [--limit N] [--jobs J]
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
    check = (f"from solution import {task['entry_point']}\n{task['test']}\n\ncheck({task['entry_point']})\n"
             "print('base tests pass')\n")
    spec.update(brief=f"Complete `{task['entry_point']}` in solution.py. Its signature and docstring are the "
                      'specification. Keep the signature, use only the standard library, and do not print.\n',
                files={'solution.py': task['prompt']}, fixtures={'check_base.py': check},
                checks=[{'name': 'base tests', 'argv': ['{python}', '-B', 'check_base.py'], 'timeout_s': 60}],
                stop_on_checks={'retain': 1})
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


def seed_all(out, limit, jobs):
    import workflow
    data, chosen = dataset(out), []
    for run in sorted((out / 'runs').glob('HumanEval-*')):
        if limit and len(chosen) >= limit:
            break
        if '.incomplete-' in run.name or not (run / 'grade.json').is_file():
            continue
        grade_result = json.loads((run / 'grade.json').read_text())
        report = json.loads((run / 'report.json').read_text(encoding='utf-8'))
        if not (report.get('stop') or {}).get('eligible') or not grade_result['round1_pass'] or grade_result.get('suspect'):
            continue  # only correct, eligible drafts: the planted defect is then the only defect
        if (out / 'seeded' / run.name / 'result.json').is_file():
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
    todo = [name for name in chosen if not (out / 'seeded' / name / 'result.json').is_file()]
    print(f'{len(chosen)} seeded drafts, {len(todo)} to review: one model call each', flush=True)

    def one(name):
        return subprocess.run([sys.executable, str(Path(__file__).resolve()), '_review', '--out', str(out),
                               '--name', name], capture_output=True, text=True)
    with concurrent.futures.ThreadPoolExecutor(jobs) as pool:
        for name, proc in zip(todo, pool.map(one, todo)):
            print(f'{name}: {proc.stdout.strip() or proc.stderr.strip()[-200:]}', flush=True)


def write_temp(out, text):
    folder = out / 'scratch'
    folder.mkdir(exist_ok=True)
    path = folder / (hashlib.sha256(text.encode()).hexdigest()[:16] + '.py')
    path.write_text(text, encoding='utf-8')
    return path


def review_one(out, name):
    """The work loop's own review of a seeded draft: the same material, plan and review role as `work`."""
    import harness
    import pipelines
    import workflow
    spec = workflow.load_spec(out / 'tasks' / name / 'task.json')
    folder, run = out / 'seeded' / name, out / 'runs' / name
    files = {'solution.py': (folder / 'solution.py').read_text(encoding='utf-8')}
    rationale = json.loads((run / 'round-1/proposal.json').read_text(encoding='utf-8'))['rationale']
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        workflow.write_files(root, {**files, **spec['fixtures']})
        observed = workflow.checks(spec, root, root, lambda: 600)
    material = workflow.review_material(spec, files, rationale, observed)
    trace = harness.outside_repo(folder / 'trace', 'seeded review')
    accounting = harness.Run(trace)
    backend = workflow.role_backend(harness.backend(spec['judge']), spec['roles'].get('review'))
    text, _ = harness.run_plan(pipelines.load_plan(None, 'review'), material, backend, accounting,
                               timeout_s=spec['timeout_s'])
    (folder / 'review.md').write_text(text + '\n', encoding='utf-8')
    report = json.loads((run / 'report.json').read_text(encoding='utf-8'))
    changed = [line.strip() for line in files['solution.py'].splitlines()
               if line.strip() and line not in (run / 'round-1/candidate/solution.py').read_text().splitlines()]
    result = {'findings': workflow.review_findings(text),
              'original_findings': report['rounds'][0].get('review_findings'),
              'mentions_change': any(line in text for line in changed), 'usage': accounting.summary()}
    (folder / 'result.json').write_text(json.dumps(result, indent=1) + '\n', encoding='utf-8')
    return result


def report_all(out, alpha):
    import shadow_labels
    rows = [shadow_labels.row(r) for r in shadow_labels.runs([out / 'runs']) if '.incomplete-' not in str(r)]
    if rows:
        print(shadow_labels.summary(rows, alpha))
    results = [json.loads(p.read_text()) for p in sorted((out / 'seeded').glob('*/result.json'))] \
        if (out / 'seeded').is_dir() else []
    if results:
        n = len(results)
        caught = sum(r['findings'] > 0 for r in results)
        located = sum(r['findings'] > 0 and r['mentions_change'] for r in results)
        paired = [r for r in results if r['original_findings'] is not None]
        alarms = sum(r['original_findings'] > 0 for r in paired)
        print(f'seeded reviews: {caught}/{n} raised a finding on the planted defect '
              f'(lower bound {1 - shadow_labels.upper_bound(n - caught, n, alpha):.1%}); '
              f'{located}/{n} quoted the changed line')
        print(f'same drafts before seeding: {alarms}/{len(paired)} reviews raised a finding (false alarms)')


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('command', choices=('run', 'grade', 'seed', 'report', '_grade', '_review'))
    parser.add_argument('--out', type=Path, required=True, help='study folder outside this checkout')
    parser.add_argument('--limit', type=int, help='first N tasks (run) or seeded drafts (seed)')
    parser.add_argument('--jobs', type=int, default=4, help='parallel runs; Codex usage limits bind near 6')
    parser.add_argument('--template', type=Path, default=HERE / 'examples' / 'v3-codex.json')
    parser.add_argument('--backend', help='writer and judge backend instead of the template\'s')
    parser.add_argument('--alpha', type=float, default=0.05)
    parser.add_argument('--task'), parser.add_argument('--file'), parser.add_argument('--name')
    args = parser.parse_args(argv)
    out = args.out.expanduser().resolve()
    if args.command == '_grade':
        task = json.loads(Path(args.task).read_text(encoding='utf-8'))
        print('PASS' if grade_here(task, Path(args.file).read_text(encoding='utf-8')) else 'FAIL')
        return 0
    if args.command == '_review':
        result = review_one(out, args.name)
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
        seed_all(out, args.limit, max(1, args.jobs))
    else:
        report_all(out, args.alpha)
    return 0


if __name__ == '__main__':
    sys.exit(main())
