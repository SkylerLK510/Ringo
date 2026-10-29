"""AGCL evaluation on HumanEval+ and MBPP+ (EvalPlus): the sealed paired-prefix study, paired arm comparisons and
the seeded-review study.

Each task becomes a `work` spec: the writer completes solution.py from the task's prompt (a HumanEval signature and
docstring, or an MBPP description with one example assert), the original tests are the public check, and EvalPlus's
extra inputs are hidden tests graded locally after the run. Runs use this checkout's workflow.py, the code under
evaluation, unless --arm names other checkouts.

  run     prepare and run tasks, J at a time (model calls: at most max_rounds + 1 per task); --dataset mbpp picks
          MBPP+, --sample N --seed S a seeded random subset in a fixed order, --arm NAME=CHECKOUT (repeatable) runs
          each task once per implementation checkout, interleaved, into runs-NAME, and --max-credits stops new
          launches before the spend at Sol rates passes the ceiling (a started pair is always finished)
  grade   score round 1 and the delivered files against the hidden tests (no model calls)
  compare paired comparison of two arms' graded runs: hidden passes, exact McNemar, tokens, credits (no model calls)
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

usage: python3 evalplus_study.py {run,grade,compare,seed,draft,probe,report} --out DIR [--limit N] [--jobs J]
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
# HumanEval/32 (find_zero): the dataset's own reference fails EvalPlus's root check on 6 of its 888 inputs, so no
# floating-point answer can be relied on to pass; it would grade noise.
DATASETS = {
    'humaneval': {'url': 'https://github.com/evalplus/humanevalplus_release/releases/download/v0.1.10/HumanEvalPlus.jsonl.gz',
                  'sha256': '272720b90ac375502c8ed23cd791c2a93dfb22a911641a494da74a426c09f101',
                  'file': 'HumanEvalPlus-v0.1.10.jsonl.gz', 'excluded': {'HumanEval/32'}},
    'mbpp': {'url': 'https://github.com/evalplus/mbppplus_release/releases/download/v0.2.0/MbppPlus.jsonl.gz',
             'sha256': 'af43697e8791c4c149bdfd6b489d8b5412507551ac20e28a439f650b8225db63',
             'file': 'MbppPlus-v0.2.0.jsonl.gz', 'excluded': set()},
}
# Sol credit rates per million tokens: uncached input, cached input, output.
RATES = {'input': 50, 'cache_read': 2.5, 'output': 250}
GRADE_TIMEOUT_S = 120
# Hidden tests sit on disk beside the specs, so the writer and reviewer must be tool-free: they cannot read files.
TOOL_FREE = ('codex-chatgpt', 'claude')


def study_dataset(out, name=None):
    """The study folder's dataset: fixed by its first `run` (study.json), so later commands cannot mix datasets."""
    path = out / 'study.json'
    recorded = json.loads(path.read_text(encoding='utf-8'))['dataset'] if path.is_file() else 'humaneval'
    if name and path.is_file() and name != recorded:
        raise ValueError(f'{out} holds the {recorded} study; use another --out for {name}')
    if name and not path.is_file():
        path.write_text(json.dumps({'dataset': name}) + '\n', encoding='utf-8')
    return name or recorded


def dataset(out, name=None):
    meta = DATASETS[study_dataset(out, name)]
    path = out / 'data' / meta['file']
    if not path.is_file():
        path.parent.mkdir(parents=True, exist_ok=True)
        with urllib.request.urlopen(meta['url'], timeout=120) as response:
            data = response.read()
        if hashlib.sha256(data).hexdigest() != meta['sha256']:
            raise ValueError(f"{meta['file']} download does not match its pinned checksum")
        path.write_bytes(data)
    elif hashlib.sha256(path.read_bytes()).hexdigest() != meta['sha256']:
        raise ValueError(f'{path} does not match the pinned checksum; delete it to download again')
    with gzip.open(path, 'rt', encoding='utf-8') as f:
        return {t['task_id']: t for t in map(json.loads, f)}


def slug(task_id):
    return task_id.replace('/', '-')


def task_of(name):
    """The task id of a run or task folder name (HumanEval-47 -> HumanEval/47, Mbpp-2 -> Mbpp/2)."""
    return name.split('.incomplete-')[0].replace('-', '/', 1)


def run_folders(runs):
    """Finished-or-not run folders of one arm, without the ones set aside as .incomplete-N."""
    return [r for r in sorted(runs.glob('*-*')) if r.is_dir() and '.incomplete-' not in r.name]


def hidden(out, task_id):
    """The task's EvalPlus record (hidden inputs and reference), one file per task so a grade loads only its own."""
    return out / 'tasks' / slug(task_id) / 'hidden.json'


def task_spec(task, template, backend=None):
    spec = json.loads(Path(template).read_text(encoding='utf-8'))
    if task['task_id'].startswith('Mbpp/'):
        # MBPP's prompt is a description with one example assert and no signature; the public check is the original
        # three asserts. math and sys are imported for the asserts that use them.
        check = (f"import math\nimport sys\nfrom solution import *\nfrom solution import {task['entry_point']}\n"
                 f"{task['assertion']}\nprint('base tests pass')\n")
        description = task['prompt'].strip().strip('"').strip()
        brief = (f"Write `{task['entry_point']}` in solution.py. The task:\n\n{description}\n\n"
                 'Use only the standard library, and do not print.\n')
        files = {'solution.py': ''}
    else:
        # The star import brings helpers the prompt defines and the test calls (HumanEval/38 and /50 call encode_*).
        check = (f"from solution import *\nfrom solution import {task['entry_point']}\n{task['test']}\n\n"
                 f"check({task['entry_point']})\n"
                 "print('base tests pass')\n")
        brief = (f"Complete `{task['entry_point']}` in solution.py. Its signature and docstring are the "
                 'specification. Keep the signature, use only the standard library, and do not print.\n')
        files = {'solution.py': task['prompt']}
    spec.update(brief=brief, files=files, fixtures={'check_base.py': check},
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


# EvalPlus's MBPP+ special oracles (evalplus/eval/_special_oracle.py and eval/__init__.py at commit 26d6d00,
# Apache-2.0): outputs compared as sets, outputs only checked for None, and four tasks with their own oracle.
MBPP_SET_EQ = {'similar_elements', 'find_char_long', 'common_in_nested_lists', 'extract_singly', 'larg_nnum',
               'intersection_array', 'find_dissimilar', 'Diff'}
MBPP_NOT_NONE = {'check_str', 'text_match_three', 'text_starta_endb'}


def _surface_area(base_edge, height):  # Mbpp/581: height is the perpendicular distance from base to apex
    slant = math.sqrt((base_edge / 2) ** 2 + height ** 2)
    return round(base_edge ** 2 + 4 * (base_edge * slant) / 2)


def _digit_distance(num1, num2):  # Mbpp/558: shorter number padded with zeros
    a, b = str(num1), str(num2)
    width = max(len(a), len(b))
    return sum(abs(int(x) - int(y)) for x, y in zip(a.zfill(width), b.zfill(width)))


def mbpp_exact(out, exp, entry_point, inp, atol):
    """EvalPlus's MBPP exact match with its special oracles."""
    exact = out == exp
    if entry_point == 'are_equivalent':  # Mbpp/164
        return True
    if entry_point == 'sum_div':  # Mbpp/295
        return exact or out == 0
    if entry_point == 'surface_Area':  # Mbpp/581
        return exact or abs(out - _surface_area(*inp)) <= atol
    if entry_point == 'digit_distance_nums':  # Mbpp/558
        return exact or out == _digit_distance(*inp)
    if entry_point in MBPP_SET_EQ:
        return set(out) == set(exp)
    if entry_point in MBPP_NOT_NONE:
        return out == exp if isinstance(out, bool) else exp == (out is not None)
    return exact


def same(out, exp, atol, entry_point, inp, mbpp=False):
    """EvalPlus's comparison: exact (with MBPP+'s special oracles), else within atol (1e-6 for float results) at
    rtol 1e-7."""
    if entry_point == 'find_zero' and not mbpp:
        xs = inp[0]
        return abs(sum(c * math.pow(out, i) for i, c in enumerate(xs))) <= atol
    if (mbpp_exact(out, exp, entry_point, inp, atol) if mbpp else out == exp):
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


def mbpp_inputs(task_id, inputs):
    """EvalPlus's mbpp_deserialize_inputs (evalplus/data/mbpp.py at 26d6d00, Apache-2.0), verbatim: JSON keeps
    tuples, sets and complex numbers as lists and strings; these tasks get them back."""
    task_id = int(task_id.split("/")[-1])
    if task_id in [
        2,
        116,
        132,
        143,
        222,
        261,
        273,
        394,
        399,
        421,
        424,
        429,
        470,
        560,
        579,
        596,
        616,
        630,
        726,
        740,
        744,
        809,
    ]:
        modified_inputs = [[tuple(lst) for lst in inp] for inp in inputs]

    elif task_id in [
        63,
        64,
        70,
        94,
        120,
        237,
        272,
        299,
        400,
        409,
        417,
        438,
        473,
        614,
        780,
    ]:
        modified_inputs = [
            [[tuple(lst) for lst in lst_lst] for lst_lst in inp] for inp in inputs
        ]

    elif task_id in [75, 413, 444, 753]:
        modified_inputs = [
            [[tuple(lst) for lst in inp[0]]] + [inp[1]] for inp in inputs
        ]

    elif task_id == 106 or task_id == 750:
        modified_inputs = [[inp[0]] + [tuple(inp[1])] for inp in inputs]

    elif task_id == 115:
        modified_inputs = [
            [
                [
                    set(item) if isinstance(item, list) and len(item) else {}
                    for item in inp[0]
                ]
            ]
            for inp in inputs
        ]

    elif task_id == 124:
        modified_inputs = [(float(inp[0]), complex(inp[1])) for inp in inputs]

    elif task_id in [250, 405, 446, 617, 720, 763, 808]:
        modified_inputs = [[tuple(inp[0])] + [inp[1]] for inp in inputs]

    elif task_id in [259, 401, 445]:
        modified_inputs = [
            [[tuple(lst) for lst in lst_lst] for lst_lst in inp] for inp in inputs
        ]
        modified_inputs = [[tuple(lst) for lst in inp] for inp in modified_inputs]

    elif task_id == 278:
        modified_inputs = [
            [[tuple(item) if isinstance(item, list) else item for item in inp[0]]]
            for inp in inputs
        ]
        modified_inputs = [[tuple(lst) for lst in inp] for inp in modified_inputs]

    elif task_id == 307:
        modified_inputs = [[tuple(inp[0])] + [inp[1], inp[2]] for inp in inputs]

    elif task_id == 722:
        modified_inputs = [
            [{key: tuple(value) for key, value in inp[0].items()}] + inp[1:]
            for inp in inputs
        ]

    elif task_id == 252:
        modified_inputs = [[complex(inp[0])] for inp in inputs]

    elif task_id in [580, 615, 791]:

        def turn_all_list_into_tuple(inp):
            if isinstance(inp, list):
                return tuple([turn_all_list_into_tuple(item) for item in inp])
            return inp

        modified_inputs = [turn_all_list_into_tuple(inp) for inp in inputs]

    else:
        modified_inputs = inputs

    return modified_inputs


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
    mbpp = task['task_id'].startswith('Mbpp/')
    inputs = list(task['base_input']) + list(task['plus_input'])  # Mbpp/793 stores its empty plus set as {}
    for inp in mbpp_inputs(task['task_id'], inputs) if mbpp else inputs:
        expected = ref(*copy.deepcopy(inp))
        if mbpp and task['entry_point'] in MBPP_NOT_NONE:  # EvalPlus's ground truth for these is "not None"
            expected = expected is not None
        try:
            if not same(fn(*copy.deepcopy(inp)), expected, atol, task['entry_point'], inp, mbpp):
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

def tasks_for(out, limit, template, backend, name=None, sample=None, seed=None):
    """The task ids to run, in run order: the dataset's order, or a seeded random sample of it (sample, seed), cut to
    the first limit. The spec of every task is written once; a rerun keeps it."""
    data = dataset(out, name)
    ids = [t for t in data if t not in DATASETS[study_dataset(out)]['excluded']]
    if sample:
        ids = random.Random(seed).sample(ids, min(sample, len(ids)))
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


def credits(usage):
    """Credits at Sol rates for a report's usage (uncached input, cached input, output)."""
    return sum((usage.get(k) or 0) * rate for k, rate in RATES.items()) / 1e6


def spent(runs):
    """Credits of every run with a report in an arm's folder, set-aside runs included: all of them were paid for."""
    return sum(credits(json.loads(r.read_text(encoding='utf-8')).get('usage') or {}) for r in runs.glob('*/report.json'))


def run_all(out, ids, jobs, arms=None, max_credits=None):
    """Run each task once per arm (name -> implementation checkout; default: this checkout into runs/). The arms of
    one task are queued together, so arms run side by side in the same task order. With max_credits, a task's first
    arm is launched only while the spend so far, the runs in flight and this task's arms, each at the mean cost of a
    finished run, stay under the ceiling; a task's other arms are always launched, so every started pair finishes."""
    arms = arms or {'': HERE}
    folders = {name: out / (f'runs-{name}' if name else 'runs') for name in arms}
    for folder in folders.values():
        folder.mkdir(exist_ok=True)
    todo = {name: set(pending(folders[name], ids)) for name in arms}
    queue = [(t, name) for t in ids for name in arms if t in todo[name]]
    print(f'{len(queue)} runs to do over {len(arms)} arm(s), {jobs} at a time; each makes at most max_rounds + 1 '
          'model calls', flush=True)
    failures, finished = 0, []

    def one(task_id, name):
        proc = subprocess.run([sys.executable, str(Path(arms[name]) / 'workflow.py'), '--spec',
                               str(out / 'tasks' / slug(task_id) / 'task.json'),
                               '--out', str(folders[name] / slug(task_id))], capture_output=True, text=True)
        return task_id, name, proc

    def affordable(active):
        if max_credits is None or queue[0][1] != next(iter(arms)):
            return True  # no ceiling, or the rest of a pair already started
        total = sum(spent(f) for f in folders.values())
        mean = sum(finished) / len(finished) if finished else 0.5  # a cautious first guess: 0.5 credits per run
        return total + mean * (len(active) + len(arms)) <= max_credits

    with concurrent.futures.ThreadPoolExecutor(jobs) as pool:
        active = set()
        while queue and len(active) < jobs and affordable(active):
            active.add(pool.submit(one, *queue.pop(0)))
        while active:
            done, active = concurrent.futures.wait(active, return_when=concurrent.futures.FIRST_COMPLETED)
            for future in done:
                task_id, name, proc = future.result()
                status = (json.loads(proc.stdout or '{}') if proc.stdout.strip().startswith('{') else {}).get('status')
                report = folders[name] / slug(task_id) / 'report.json'
                if report.is_file():
                    finished.append(credits(json.loads(report.read_text(encoding='utf-8')).get('usage') or {}))
                print(f'{task_id}{" [" + name + "]" if name else ""}: {status or "failed"} '
                      f'{proc.stderr.strip()[-200:] if not status else ""}', flush=True)
                # Three runs in a row that end in an error (a usage limit, a lost login) stop new launches.
                failures = failures + 1 if status in RETRY else 0
            while queue and failures < 3 and len(active) < jobs and affordable(active):
                active.add(pool.submit(one, *queue.pop(0)))
            if failures >= 3:
                print('stopping: three runs in a row failed; fix the cause and rerun to continue', flush=True)
                queue.clear()
    if queue:
        print(f'stopped at the credit ceiling with {len(queue)} runs not started; spent '
              f'{sum(spent(f) for f in folders.values()):.2f} of {max_credits}', flush=True)


def grade_all(out):
    count = 0
    for run in [r for runs in sorted(out.glob('runs*')) if runs.is_dir() for r in run_folders(runs)]:
        if not (run / 'report.json').is_file():
            continue
        task_id = task_of(run.name)
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
    for run in run_folders(out / 'runs'):
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
        task_id = task_of(run.name)
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
    names = [r.name for r in run_folders(out / 'runs') if (r / 'grade.json').is_file()]
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
    for run in run_folders(out / 'runs'):
        if '.incomplete-' in run.name or not (run / 'grade.json').is_file():
            continue
        task = json.loads(hidden(out, task_of(run.name)).read_text(encoding='utf-8'))
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
    for run in run_folders(out / 'runs'):
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


def mcnemar_exact(gains, harms):
    """Two-sided exact McNemar p: the binomial test of the discordant pairs at one half."""
    n = gains + harms
    if not n:
        return 1.0
    return min(1.0, 2 * sum(math.comb(n, k) for k in range(min(gains, harms) + 1)) / 2 ** n)


def paired_ci(diffs, resamples=10_000, seed=0, level=0.95):
    """Percentile bootstrap interval of the mean of paired differences (tasks resampled with replacement)."""
    rng, n = random.Random(seed), len(diffs)
    means = sorted(sum(diffs[rng.randrange(n)] for _ in range(n)) / n for _ in range(resamples))
    return means[int((1 - level) / 2 * resamples)], means[int((1 + level) / 2 * resamples) - 1]


def arm_row(run):
    """One graded run's numbers for a comparison."""
    report = json.loads((run / 'report.json').read_text(encoding='utf-8'))
    grade_result = json.loads((run / 'grade.json').read_text(encoding='utf-8'))
    usage = report.get('usage') or {}
    rounds = report.get('rounds') or []
    first = rounds[0] if rounds else {}
    checks_pass = bool(first.get('checks')) and all(c['status'] == 'pass' for c in first['checks'])
    # stands: a revision that left code failing a declared check unchanged (what the dispute rule stops on)
    stands = [r['round'] for prev, r in zip(rounds, rounds[1:]) if prev.get('checks') and r.get('candidate_sha256')
              and r.get('candidate_sha256') == prev.get('candidate_sha256')
              and any(c['status'] != 'pass' for c in prev['checks'])]
    import workflow
    notes = [r['round'] for r in rounds if (run / f"round-{r['round']}" / 'proposal.json').is_file()
             and workflow.special_cases(json.loads((run / f"round-{r['round']}" / 'proposal.json')
                                                   .read_text(encoding='utf-8'))['files'])]
    return {'pass': grade_result['delivered_pass'], 'round1_pass': grade_result['round1_pass'],
            'suspect': grade_result.get('suspect', False), 'status': report['status'],
            'tokens': sum(usage.get(k) or 0 for k in ('input', 'cache_read', 'cache_write', 'output')),
            'credits': credits(usage), 'calls': usage.get('calls') or 0, 'rounds': len(rounds),
            'round1_checks_pass': checks_pass, 'round1_findings': first.get('review_findings'),
            'stands': stands, 'noted_rounds': notes}


def compare_all(out, arms, alpha):
    """Paired comparison of two arms on the tasks both graded, suspect drafts left out (no model calls)."""
    a, b = arms
    rows = {}
    for name in arms:
        rows[name] = {task_of(r.name): arm_row(r) for r in run_folders(out / f'runs-{name}')
                      if (r / 'grade.json').is_file()}
    both = sorted(set(rows[a]) & set(rows[b]))
    suspect = [t for t in both if rows[a][t]['suspect'] or rows[b][t]['suspect']]
    paired = [t for t in both if t not in suspect]
    A, B = [rows[a][t] for t in paired], [rows[b][t] for t in paired]
    n = len(paired)
    if not n:
        print('no paired graded tasks')
        return {}
    result = {'arms': [a, b], 'paired': n, 'suspect': suspect,
              'unpaired': sorted(set(rows[a]) ^ set(rows[b]))}
    for key in ('pass', 'round1_pass'):
        gains = [t for t, x, y in zip(paired, A, B) if y[key] and not x[key]]
        harms = [t for t, x, y in zip(paired, A, B) if x[key] and not y[key]]
        diffs = [int(y[key]) - int(x[key]) for x, y in zip(A, B)]
        result[key] = {a: sum(x[key] for x in A), b: sum(y[key] for y in B), 'gains': gains, 'harms': harms,
                       'mcnemar_p': mcnemar_exact(len(gains), len(harms)), 'diff_ci': paired_ci(diffs)}
    for key in ('tokens', 'credits', 'calls'):
        diffs = [y[key] - x[key] for x, y in zip(A, B)]
        result[key] = {a: sum(x[key] for x in A) / n, b: sum(y[key] for y in B) / n,
                       'diff': sum(diffs) / n, 'diff_ci': paired_ci(diffs)}
    for name, rs in ((a, A), (b, B)):
        statuses = {}
        for r in rs:
            statuses[r['status']] = statuses.get(r['status'], 0) + 1
        reviewed = [r for r in rs if r['round1_checks_pass'] and r['round1_findings'] is not None]
        result[f'{name}_detail'] = {
            'statuses': statuses,
            'stands': {t: r['stands'] for t, r in zip(paired, rs) if r['stands']},
            'noted': {t: r['noted_rounds'] for t, r in zip(paired, rs) if r['noted_rounds']},
            # round-1 review: findings on drafts that fail the hidden tests (catches) and that pass them (alarms)
            'review_catch': [sum(bool(r['round1_findings']) for r in reviewed if not r['round1_pass']),
                             sum(not r['round1_pass'] for r in reviewed)],
            'review_alarm': [sum(bool(r['round1_findings']) for r in reviewed if r['round1_pass']),
                             sum(bool(r['round1_pass']) for r in reviewed)]}
    p, r1 = result['pass'], result['round1_pass']
    print(f"paired tasks {n} ({len(suspect)} suspect left out, {len(result['unpaired'])} unpaired)")
    print(f"hidden pass, delivered: {a} {p[a]}/{n}, {b} {p[b]}/{n}; gains {len(p['gains'])}, harms "
          f"{len(p['harms'])}, exact McNemar p = {p['mcnemar_p']:.3g}; difference 95% CI "
          f"[{p['diff_ci'][0]:+.3f}, {p['diff_ci'][1]:+.3f}]")
    print(f"hidden pass, round 1: {a} {r1[a]}/{n}, {b} {r1[b]}/{n}; gains {len(r1['gains'])}, harms "
          f"{len(r1['harms'])}, p = {r1['mcnemar_p']:.3g}")
    for key in ('tokens', 'credits', 'calls'):
        x = result[key]
        print(f"{key} per task: {a} {x[a]:,.3f}, {b} {x[b]:,.3f}; {b} - {a} = {x['diff']:+,.3f} "
              f"(95% CI [{x['diff_ci'][0]:+,.3f}, {x['diff_ci'][1]:+,.3f}])")
    for name in arms:
        d = result[f'{name}_detail']
        print(f"{name}: statuses {d['statuses']}; stands {len(d['stands'])}; static notes {len(d['noted'])}; "
              f"round-1 review caught {d['review_catch'][0]}/{d['review_catch'][1]} hidden failures, "
              f"alarmed on {d['review_alarm'][0]}/{d['review_alarm'][1]} hidden passes")
    print('gains:', ', '.join(p['gains']) or 'none', '| harms:', ', '.join(p['harms']) or 'none')
    (out / f'compare-{a}-{b}.json').write_text(json.dumps(result, indent=1) + '\n', encoding='utf-8')
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('command', choices=('run', 'grade', 'compare', 'seed', 'draft', 'probe', 'report', '_grade',
                                            '_review', '_probe', '_draft'))
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
    parser.add_argument('--dataset', choices=sorted(DATASETS), help='run: the dataset (fixed per study folder)')
    parser.add_argument('--sample', type=int, help='run: a random sample of N tasks, in a fixed order')
    parser.add_argument('--seed', type=int, default=0, help='run: the seed of --sample')
    parser.add_argument('--arm', action='append', metavar='NAME=CHECKOUT',
                        help='run: an arm, the implementation checkout whose workflow.py runs into runs-NAME; '
                             'compare: two arm names')
    parser.add_argument('--max-credits', type=float, help='run: stop new launches before this spend (Sol rates)')
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
        arms = None
        if args.arm:
            arms = dict(a.split('=', 1) for a in args.arm)
            for name, checkout in arms.items():
                if not name.isidentifier() or not (Path(checkout) / 'workflow.py').is_file():
                    parser.error(f'--arm {name}={checkout}: needs a name and a Ringo checkout')
                arms[name] = Path(checkout).expanduser().resolve()
        ids = tasks_for(out, args.limit, args.template, args.backend, args.dataset, args.sample, args.seed)
        run_all(out, ids, max(1, args.jobs), arms, args.max_credits)
    elif args.command == 'grade':
        grade_all(out)
    elif args.command == 'compare':
        if not args.arm or len(args.arm) != 2:
            parser.error('compare needs two --arm NAME')
        compare_all(out, args.arm, args.alpha)
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
