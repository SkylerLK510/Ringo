"""Offline turn-run driver: task fixtures, budgets, a hash-chained ledger and outcome classes.

The runner binary (`cli/src/bin/turnrun.rs`, built with `cargo build --features turnrun`) runs one
shell turn through the real `Session` and writes one JSON record. This module is the campaign layer
around it. It makes no model call of its own and takes no improvement decision. See
docs/TURN-RUNNER.md.

The task inventory is private. Its briefs, and above all its checks, are what a task agent must not
see, so it lives outside this repository and is named with --tasks DIR (or TURNRUN_TASKS):
DIR/tasks.json plus the checks it names. The protocol a check follows is in docs/TURN-RUNNER.md.

Commands:
  inventory                                  validate the inventory against the source history
  fixture TASK --side base|fix --out DIR     rebuild one task environment
  verify-inventory --work DIR [--ids T01,..] [--report PATH]
                                             each check must fail at base and pass at fix
  run TASK --campaign DIR --agent NAME --program PATH --budget-tokens N --budget-seconds N
      [--rep N] [--warmup] [--total-budget-tokens N] [--mode ask|auto|read] [--retry-interrupted]
      [--no-check] [--binary PATH]           one offline run, ledgered
  split --dev-until YYYY-MM-DD [--out PATH]  dev/validation by date and cluster
  totals CAMPAIGN                            recount a campaign's ledger

What it enforces:
- Offline only. A run needs --program, and a program that is an installed agent CLI is refused:
  the agent's program is its transport to a provider, and nothing here separates that transport from
  the task's network access. This is a tripwire against an accidental live run, not isolation.
- Fixtures without history. A fixture is the source commit's tree, made one commit with a fixed
  identity, date and message: no remote, no alternates, no later objects, no source commit id.
- One count per key. A run key (task, rep, warm-up) is ledgered once. An attempt that started and
  never finished blocks its key until --retry-interrupted, once; its usage is unknown.
- A pinned campaign. The first run pins the runner, the agent program, the inventory with its checks,
  this driver and the interpreter; a later run whose pins differ is refused. A task's fixture must
  rebuild to the same commit every time.
- Budgets that count everything. Usage is input + cache_write + cache_read + output, warm-ups
  included. Usage a run did not report is unknown, never zero: the ledger reserves that run's whole
  cap for it. With --total-budget-tokens, a run starts only if what is spent or reserved plus its own
  cap fits. A post-turn backend can overshoot its cap (the runner records `cap_overshoot`).
- Private traces. Campaigns, work folders and reports must be outside this repository.
"""
import argparse
import hashlib
import io
import json
import os
import re
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import time
from datetime import date, datetime, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parent
EXE = '.exe' if os.name == 'nt' else ''
DEFAULT_BINARY = REPO / 'cli' / 'target' / 'debug' / f'turnrun{EXE}'
SHA = re.compile(r'[0-9a-f]{40}')
TASK_ID = re.compile(r'T\d{2,}')
RUNTIMES = ('python', 'rust', 'rust-pty')
LIVE_AGENTS = ('claude', 'codex', 'agy', 'antigravity', 'opencode', 'gemini', 'ringo')
# A reason that names an exhausted plan is a pause for the provider's quota, not a verdict on the
# turn. Matched against the runner record's reason.
QUOTA = re.compile(r'rate.?limit|usage limit|quota|hit your .{0,20}limit|limit reached|resets? at|'
                   r'insufficient.{0,10}(credit|balance)', re.I)
CHECK_CLASSES = {0: 'pass', 1: 'treatment_failure', 2: 'quality_failure', 3: 'check_unavailable'}
CHECK_TIMEOUT = 45 * 60  # a Rust check builds the tree
RUNNER_GRACE = 120  # past the run's own wall cap, before the driver stops the runner itself
SCRUB = ('RINGO_CHILD', 'RINGO_HOME', 'CODEX_HOME', 'ANTHROPIC_API_KEY', 'OPENAI_API_KEY',
         'GEMINI_API_KEY', 'GOOGLE_API_KEY')
USAGE_KEYS = ('input', 'cache_write', 'cache_read', 'output')


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1 << 16), b''):
            h.update(chunk)
    return h.hexdigest()


def now():
    return datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')


def outside_repo(path, what):
    """`path` resolved, refused if it is this repository or inside it: tasks and traces are private."""
    resolved = Path(path).resolve()
    if resolved == REPO or REPO in resolved.parents:
        raise ValueError(f'{what} {resolved} is inside the repository; it must stay outside it')
    return resolved


def git(repo, *args, env=None, timeout=300):
    result = subprocess.run(['git', '-C', str(repo), *args], capture_output=True, text=True, encoding='utf-8',
                            errors='replace', env=env, timeout=timeout)
    if result.returncode != 0:
        raise RuntimeError(f'git {" ".join(args)}: {result.stderr.strip()}')
    return result.stdout.strip()


def clean_env(extra=None):
    """The caller's environment without Ringo's and the providers' variables: a check's result and
    a fixture's setup must not depend on the shell that started the driver."""
    env = {k: v for k, v in os.environ.items() if k not in SCRUB and not k.startswith('GIT_')
           and not k.endswith(('API_KEY', 'AUTH_TOKEN'))}
    env.update(extra or {})
    return env


# --- inventory -----------------------------------------------------------------------------------

def tasks_dir(arg):
    named = arg or os.environ.get('TURNRUN_TASKS')
    if not named:
        raise ValueError('name the private task inventory with --tasks DIR or TURNRUN_TASKS')
    return outside_repo(named, 'the task inventory')


def load_inventory(tasks, source=None, need_checks=True):
    """The inventory's tasks, validated. With `source`, every commit is checked against its history:
    it exists, the first fix's parent is the task's parent, the fixes are in order, and the task's
    date is its last fix's date (the split sorts on it)."""
    tasks = Path(tasks)
    body = json.loads((tasks / 'tasks.json').read_text(encoding='utf-8'))
    items = body.get('tasks')
    if not isinstance(items, list) or not items:
        raise ValueError('tasks.json needs a non-empty "tasks" list')
    seen = set()
    for t in items:
        tid = t.get('id', '')
        if not TASK_ID.fullmatch(tid) or tid in seen:
            raise ValueError(f'{tid!r}: a task id is T and two or more digits, and unique')
        seen.add(tid)
        for key in ('cluster', 'brief', 'check', 'title'):
            if not isinstance(t.get(key), str) or not t[key].strip():
                raise ValueError(f'{tid}: {key} is missing')
        if t.get('runtime') not in RUNTIMES:
            raise ValueError(f'{tid}: runtime must be one of {", ".join(RUNTIMES)}')
        date.fromisoformat(t.get('date', ''))
        fixes = t.get('fix_commits')
        if not isinstance(fixes, list) or not fixes:
            raise ValueError(f'{tid}: fix_commits is missing')
        for sha in [t.get('parent_commit', ''), *fixes]:
            if not isinstance(sha, str) or not SHA.fullmatch(sha):
                raise ValueError(f'{tid}: {sha!r} is not a full 40-character commit id')
            if sha[:7] in t['brief'].lower():
                raise ValueError(f'{tid}: the brief carries a commit id')
        if t['parent_commit'] in fixes:
            raise ValueError(f'{tid}: the parent is also a fix')
        check = (tasks / t['check']).resolve()
        if tasks.resolve() not in check.parents:
            raise ValueError(f'{tid}: the check must be inside the inventory folder')
        if need_checks and not check.is_file():
            raise ValueError(f'{tid}: check {t["check"]} is missing')
        if source is not None:
            check_history(t, source)
    return items


def check_history(t, source):
    tid = t['id']
    for sha in [t['parent_commit'], *t['fix_commits']]:
        if subprocess.run(['git', '-C', str(source), 'cat-file', '-e', f'{sha}^{{commit}}'],
                          capture_output=True).returncode != 0:
            raise ValueError(f'{tid}: {sha} is not in {source}')
    if git(source, 'rev-parse', f'{t["fix_commits"][0]}^') != t['parent_commit']:
        raise ValueError(f'{tid}: the parent commit is not the first fix\'s parent')
    for a, b in zip(t['fix_commits'], t['fix_commits'][1:]):
        if subprocess.run(['git', '-C', str(source), 'merge-base', '--is-ancestor', a, b],
                          capture_output=True).returncode != 0:
            raise ValueError(f'{tid}: fix {b[:12]} does not follow {a[:12]}')
    when = git(source, 'log', '-1', '--format=%cs', t['fix_commits'][-1])
    if when != t['date']:
        raise ValueError(f'{tid}: date {t["date"]} is not its last fix\'s date {when}')


def inventory_digest(tasks, items):
    """One hash over tasks.json and every check file it names: a campaign pins it."""
    h = hashlib.sha256((Path(tasks) / 'tasks.json').read_bytes())
    for t in sorted(items, key=lambda t: t['id']):
        h.update(t['check'].encode('utf-8'))
        h.update(bytes.fromhex(sha256_file(Path(tasks) / t['check'])))
    return h.hexdigest()


def by_id(items, task_id):
    for t in items:
        if t['id'] == task_id:
            return t
    raise ValueError(f'unknown task {task_id}')


# --- fixtures ------------------------------------------------------------------------------------

FIXTURE_ENV = {
    'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_CONFIG_NOSYSTEM': '1',
    'GIT_AUTHOR_NAME': 'task', 'GIT_AUTHOR_EMAIL': 'task@invalid', 'GIT_AUTHOR_DATE': '2026-01-01T00:00:00Z',
    'GIT_COMMITTER_NAME': 'task', 'GIT_COMMITTER_EMAIL': 'task@invalid',
    'GIT_COMMITTER_DATE': '2026-01-01T00:00:00Z',
}


def side_commit(task, side):
    return task['parent_commit'] if side == 'base' else task['fix_commits'][-1]


def build_fixture(source, sha, out):
    """A task environment holding `sha`'s tree as one commit with a fixed identity, date and message.
    It has no remote, no alternates and no other objects, so the fix cannot be reached through git,
    and the same tree always rebuilds to the same commit, so a rerun can prove its setup."""
    out = Path(out)
    if out.exists() and any(out.iterdir()):
        raise ValueError(f'{out} is not empty')
    out.mkdir(parents=True, exist_ok=True)
    env = clean_env(FIXTURE_ENV)  # no global or system config: no line-ending conversion on the way
    archive = subprocess.run(['git', '-C', str(source), 'archive', '--format=tar', sha],
                             capture_output=True, env=env, timeout=600)
    if archive.returncode != 0:
        raise RuntimeError(f'git archive {sha}: {archive.stderr.decode(errors="replace").strip()}')
    with tarfile.open(fileobj=io.BytesIO(archive.stdout)) as tar:
        try:
            tar.extractall(out, filter='data')
        except TypeError:  # an interpreter without extraction filters
            tar.extractall(out)
    git(out, 'init', '-q', '--template=', env=env)
    git(out, 'add', '-A', env=env)
    git(out, 'commit', '-q', '--no-verify', '--no-gpg-sign', '-m', 'task snapshot', env=env)
    if git(out, 'rev-list', '--all', '--count', env=env) != '1':
        raise RuntimeError('the fixture must hold exactly one commit')
    if git(out, 'remote', env=env) or (out / '.git' / 'objects' / 'info' / 'alternates').exists():
        raise RuntimeError('the fixture must have no remote and no alternates')
    # The same tree, byte for byte: an export attribute or a line-ending rule that changed a file on
    # the way would make the task something other than the commit it names.
    tree = git(out, 'rev-parse', 'HEAD^{tree}', env=env)
    if tree != git(source, 'rev-parse', f'{sha}^{{tree}}'):
        raise RuntimeError(f'the fixture\'s tree is not {sha[:12]}\'s tree')
    if git(out, 'status', '--porcelain', '--untracked-files=all', env=env):
        raise RuntimeError('the fixture is not clean after its commit')
    return {'dir': str(out), 'source_commit': sha, 'setup_commit': git(out, 'rev-parse', 'HEAD', env=env),
            'setup_tree': tree}


# --- checks and classes --------------------------------------------------------------------------

def run_check(check, fixture, log):
    """The task's protected check against `fixture`, with this interpreter and a clean environment.
    Its exit code, and everything it printed in `log`."""
    try:
        proc = subprocess.run([sys.executable, str(check), str(fixture)], capture_output=True, text=True,
                              encoding='utf-8', errors='replace', env=clean_env(), timeout=CHECK_TIMEOUT)
        code, text = proc.returncode, proc.stdout + proc.stderr
    except subprocess.TimeoutExpired as exc:
        code, text = None, f'the check passed its {CHECK_TIMEOUT}s limit\n{exc.stdout or ""}'
    Path(log).write_text(f'exit {code}\n{text}', encoding='utf-8')
    lines = [l for l in text.strip().splitlines() if l.strip()]
    return code, (lines[-1] if lines else '')


def classify(record, check_exit=None, checked=True):
    """The runner's record and the check's exit code, as one class.

    A finished turn is judged by its check: pass, treatment_failure (the symptom remains),
    quality_failure (fixed, but a named regression test fails), check_unavailable, or check_error.
    A turn stopped at a cap or a wall is cap_breach or timeout. A turn that failed, was cancelled or
    was refused is a delivery_failure, or a quota_pause when its reason names the provider's quota.
    """
    outcome = record.get('outcome')
    if outcome == 'done':
        if not checked:
            return 'unchecked'
        return CHECK_CLASSES.get(check_exit, 'check_error')
    if outcome in ('cap_breach', 'timeout'):
        return outcome
    reason = json.dumps(record.get('reason'))
    return 'quota_pause' if QUOTA.search(reason) else 'delivery_failure'


def usage_of(record):
    """(usage, total) from a runner record, total None when the usage is unknown. A run the runner
    refused before launching made no call: zero. A launched run that reported no count, or had a
    call that reported none, is unknown: its count is at best a lower bound."""
    u = record.get('usage')
    if u is None:
        return ({k: 0 for k in USAGE_KEYS}, 0) if record.get('outcome') == 'refused' else (None, None)
    counts = {k: u.get(k) for k in USAGE_KEYS}
    if any(type(v) is not int or v < 0 for v in counts.values()):
        raise ValueError('the runner reported invalid usage counters')
    known = bool(u.get('counted')) and not u.get('uncounted_calls')
    return counts, (sum(counts.values()) if known else None)


# --- ledger --------------------------------------------------------------------------------------

def ledger_entries(path):
    """The ledger's entries, its chain verified: each line names the sha256 of the line before it,
    so a changed, removed or reordered line is found. Every run's record file must still hash to
    what was ledgered."""
    path = Path(path)
    if not path.exists():
        return []
    entries, prev = [], ''
    for i, line in enumerate(path.read_text(encoding='utf-8').splitlines(), 1):
        entry = json.loads(line)
        if entry.get('prev') != prev or entry.get('seq') != i:
            raise ValueError(f'ledger line {i} breaks the chain')
        prev = sha256_bytes(line.encode('utf-8'))
        if entry.get('kind') == 'run' and entry.get('record_sha256'):
            record = Path(entry['record_path'])
            if not record.is_file() or sha256_file(record) != entry['record_sha256']:
                raise ValueError(f'ledger line {i}: its record {record} is gone or changed')
        entries.append(entry)
    return entries


def ledger_append(path, entry):
    """One line, chained to the last, written whole and synced: a crash leaves it in or out."""
    path = Path(path)
    lines = path.read_text(encoding='utf-8').splitlines() if path.exists() else []
    entry = {**entry, 'seq': len(lines) + 1, 'prev': sha256_bytes(lines[-1].encode('utf-8')) if lines else '',
             'time': entry.get('time') or now()}
    with open(path, 'a', encoding='utf-8', newline='\n') as f:
        f.write(json.dumps(entry, sort_keys=True) + '\n')
        f.flush()
        os.fsync(f.fileno())
    return entry


def attempts(entries):
    """Per key: its finished run, the starts with no run after them, and how many were abandoned."""
    keys = {}
    for e in entries:
        k = keys.setdefault(e.get('key'), {'run': None, 'open': None, 'abandoned': 0})
        if e['kind'] == 'start':
            k['open'] = e
        elif e['kind'] == 'run':
            k['run'], k['open'] = e, None
        elif e['kind'] == 'abandoned':
            k['open'], k['abandoned'] = None, k['abandoned'] + 1
    return keys


def totals_of(entries):
    """Every token the campaign spent or must assume it spent. `known` is reported usage, warm-ups
    included (`warmup` is their share). `reserved` is the whole cap of each run whose usage is
    unknown: unreported, abandoned, or still open. `committed` = known + reserved is what a total
    budget is checked against."""
    known = warmup = reserved = without_cache_read = 0
    unknown_runs = 0
    for e in entries:
        if e['kind'] == 'run':
            if e.get('usage_total') is None:
                reserved += max(e['budget']['tokens'], sum((e.get('usage') or {}).values()))
                unknown_runs += 1
            else:
                known += e['usage_total']
                without_cache_read += e['usage_total'] - e['usage']['cache_read']
                warmup += e['usage_total'] if e.get('warmup') else 0
        elif e['kind'] == 'abandoned':
            reserved += e['budget']['tokens']
            unknown_runs += 1
    for k in attempts(entries).values():
        if k['open']:
            reserved += k['open']['budget']['tokens']
            unknown_runs += 1
    return {'known': known, 'warmup': warmup, 'known_without_cache_read': without_cache_read,
            'reserved': reserved, 'committed': known + reserved, 'unknown_runs': unknown_runs}


class CampaignLock:
    """One driver per campaign: a lock file made exclusively. A driver that died leaves it; check
    that none is running before removing it."""

    def __init__(self, campaign):
        self.path = Path(campaign) / '.lock'

    def __enter__(self):
        try:
            fd = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        except FileExistsError:
            raise ValueError(f'{self.path} exists: another driver is running this campaign, or one died; '
                             'remove it only after checking') from None
        os.write(fd, str(os.getpid()).encode())
        os.close(fd)
        return self

    def __exit__(self, *exc):
        self.path.unlink(missing_ok=True)


# --- run -----------------------------------------------------------------------------------------

def live_program(program, agent):
    """Whether `program` is an installed agent CLI (a live session), by name or by resolved path."""
    names = {*LIVE_AGENTS, agent}
    if program.stem.lower() in names:
        return True
    for name in names:
        found = shutil.which(name)
        if found and Path(found).resolve() == program.resolve():
            return True
    return False


def runner_command(binary):
    return [sys.executable, str(binary)] if binary.suffix == '.py' else [str(binary)]


def driver_identity():
    try:
        commit = git(REPO, 'rev-parse', 'HEAD')
        dirty = bool(git(REPO, 'status', '--porcelain', '--', 'turnrun.py'))
    except (RuntimeError, OSError):
        commit, dirty = None, None
    return {'commit': commit, 'turnrun_py_dirty': dirty, 'sha256': sha256_file(Path(__file__))}


def stop_tree(proc):
    """Stop the runner and what it started: its own process group (POSIX) or tree (Windows), never
    a kill by name."""
    try:
        if os.name == 'nt':
            subprocess.run(['taskkill', '/T', '/F', '/PID', str(proc.pid)], capture_output=True, timeout=60)
        else:
            # Session's agents have their own process groups. Let the runner's signal handler
            # shut those groups down before using the last-resort stop on the runner itself.
            os.killpg(proc.pid, signal.SIGTERM)
            try:
                proc.wait(timeout=10)
                return
            except subprocess.TimeoutExpired:
                pass
            os.killpg(proc.pid, signal.SIGKILL)
    except (OSError, subprocess.SubprocessError):
        proc.kill()


def run_runner(cmd, limit, log):
    kwargs = {'creationflags': subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == 'nt' else {'start_new_session': True}
    with open(log, 'w', encoding='utf-8') as out:
        proc = subprocess.Popen(cmd, stdout=out, stderr=subprocess.STDOUT, env=clean_env(), **kwargs)
        try:
            return proc.wait(timeout=limit), False
        except subprocess.TimeoutExpired:
            stop_tree(proc)
            proc.wait()
            return proc.returncode, True
        except BaseException:
            stop_tree(proc)
            proc.wait()
            raise


def slug(key):
    return key.replace(':', '-')


def run_task(args):
    if 'RINGO_CHILD' in os.environ:
        # The runner refuses under an agent Ringo launched; the driver must not launder that away by
        # starting it with a clean environment.
        raise ValueError('RINGO_CHILD is set: an agent Ringo started does not start runs of its own')
    tasks = tasks_dir(args.tasks)
    source = Path(args.source).resolve()
    items = load_inventory(tasks, source)
    task = by_id(items, args.task)
    campaign = outside_repo(args.campaign, 'the campaign folder')
    program = Path(args.program).resolve()
    binary = Path(args.binary).resolve() if args.binary else DEFAULT_BINARY
    if not program.is_file():
        raise ValueError(f'{program} is not a file')
    if live_program(program, args.agent):
        raise ValueError(f'{program} is an installed agent CLI: live runs are refused until the provider '
                         'transport is separated from the task\'s network access')
    if not binary.is_file():
        raise ValueError(f'the runner {binary} is missing: (cd cli && cargo build --locked --features turnrun)')
    if args.budget_tokens <= 0 or args.budget_seconds <= 0:
        raise ValueError('--budget-tokens and --budget-seconds must be positive')
    if not re.fullmatch(r'[1-9]\d*', args.rep):
        raise ValueError('--rep must be a positive integer')
    if args.total_budget_tokens is not None and args.total_budget_tokens <= 0:
        raise ValueError('--total-budget-tokens must be positive')
    campaign.mkdir(parents=True, exist_ok=True)
    ledger = campaign / 'ledger.jsonl'
    with CampaignLock(campaign):
        entries = ledger_entries(ledger)
        pins = {'agent': args.agent, 'mode': args.mode, 'runner_sha256': sha256_file(binary),
                'program_sha256': sha256_file(program), 'inventory_sha256': inventory_digest(tasks, items),
                'driver': driver_identity(), 'python': sys.version.split()[0],
                'source_head': git(source, 'rev-parse', 'HEAD'),
                'total_budget_tokens': args.total_budget_tokens, 'policy_sha256': None}
        if args.policy:
            from turnrun_campaign import policy
            policy(json.loads(Path(args.policy).read_text(encoding='utf-8')))
            pins['policy_sha256'] = sha256_file(args.policy)
        pinned = next((e for e in entries if e['kind'] == 'campaign'), None)
        if pinned is None:
            ledger_append(ledger, {'kind': 'campaign', 'pins': pins})
        else:
            changed = sorted(k for k in pins if k != 'source_head' and pins[k] != pinned['pins'].get(k))
            if changed:
                raise ValueError(f'the campaign\'s pinned setup changed: {", ".join(changed)}')

        key = f'{task["id"]}:{args.rep}' + (':warmup' if args.warmup else '')
        run_dir = campaign / 'runs' / slug(key)
        k = attempts(entries).get(key, {'run': None, 'open': None, 'abandoned': 0})
        if k['run']:
            raise ValueError(f'run {key} is already ledgered; a key runs once')
        if k['open']:
            if not args.retry_interrupted:
                raise ValueError(f'run {key} started and never finished; its folder is evidence. '
                                 '--retry-interrupted abandons it (its cap stays reserved) and runs again, once')
            if k['abandoned']:
                raise ValueError(f'run {key} was already retried once')
            kept = run_dir.with_name(f'{run_dir.name}.abandoned')
            if run_dir.exists():
                run_dir.rename(kept)
            ledger_append(ledger, {'kind': 'abandoned', 'key': key, 'budget': k['open']['budget'],
                                   'moved_to': str(kept)})
            entries = ledger_entries(ledger)
        if args.total_budget_tokens is not None:
            committed = totals_of(entries)['committed']
            if committed + args.budget_tokens > args.total_budget_tokens:
                ledger_append(ledger, {'kind': 'budget_stop', 'key': key, 'committed': committed,
                                       'cap': args.budget_tokens, 'total': args.total_budget_tokens})
                print(json.dumps({'class': 'budget_stop', 'key': key, 'committed': committed,
                                  'total': args.total_budget_tokens}))
                return 3
        if run_dir.exists():
            raise ValueError(f'{run_dir} exists but the ledger has no attempt for {key}; move it aside')

        fixture = build_fixture(source, task['parent_commit'], run_dir / 'fixture')
        earlier = next((e for e in entries if e['kind'] == 'start' and e.get('task') == task['id']), None)
        if earlier and earlier['setup_commit'] != fixture['setup_commit']:
            raise ValueError(f'{task["id"]}\'s fixture rebuilt to {fixture["setup_commit"][:12]}, '
                             f'not {earlier["setup_commit"][:12]} as before')
        brief = run_dir / 'brief.txt'
        brief.write_text(task['brief'] + '\n', encoding='utf-8')
        budget = {'tokens': args.budget_tokens, 'seconds': args.budget_seconds, 'total': args.total_budget_tokens}
        ledger_append(ledger, {'kind': 'start', 'key': key, 'task': task['id'], 'warmup': args.warmup,
                               'source_commit': fixture['source_commit'], 'setup_commit': fixture['setup_commit'],
                               'setup_tree': fixture['setup_tree'], 'budget': budget})

        record_path = run_dir / 'record.json'
        cmd = [*runner_command(binary), '--project', fixture['dir'], '--agent', args.agent,
               '--program', str(program), '--message-file', str(brief), '--mode', args.mode,
               '--state', str(run_dir / 'state'), '--home', str(run_dir / 'home'), '--source-id', key,
               '--record', str(record_path), '--budget-tokens', str(args.budget_tokens),
               '--budget-seconds', str(args.budget_seconds)]
        if args.policy:
            cmd += ['--policy', str(Path(args.policy).resolve())]
        code, stopped = run_runner(cmd, args.budget_seconds + RUNNER_GRACE, run_dir / 'runner.log')
        if record_path.is_file():
            record = json.loads(record_path.read_text(encoding='utf-8'))
        else:
            why = 'the driver stopped the runner past its wall cap' if stopped else f'the runner exited {code}'
            record = {'outcome': 'failed', 'reason': f'{why} and wrote no record'}
            record_path.write_text(json.dumps(record) + '\n', encoding='utf-8')
        if stopped:
            record = {**record, 'runner_outcome': record.get('outcome'), 'outcome': 'timeout',
                      'driver_stop': 'the runner exceeded its wall cap and shutdown grace'}
            record_path.write_text(json.dumps(record) + '\n', encoding='utf-8')

        check_exit, evidence, checked = None, '', record.get('outcome') == 'done' and not args.no_check
        if checked:
            check_exit, evidence = run_check(tasks / task['check'], fixture['dir'], run_dir / 'check.txt')
        usage, total = usage_of(record)
        outcome_class = classify(record, check_exit, checked)
        ledger_append(ledger, {
            'kind': 'run', 'key': key, 'task': task['id'], 'warmup': args.warmup, 'class': outcome_class,
            'outcome': record.get('outcome'), 'runner_exit': code, 'check_exit': check_exit,
            'evidence': evidence, 'usage': usage, 'usage_total': total,
            'cap_overshoot': record.get('cap_overshoot'), 'stopped_by': record.get('stopped_by'),
            'elapsed_s': record.get('elapsed_s'), 'budget': budget,
            'record_path': str(record_path), 'record_sha256': sha256_file(record_path)})
    print(json.dumps({'key': key, 'class': outcome_class, 'outcome': record.get('outcome'),
                      'usage_total': total, 'check_exit': check_exit, 'evidence': evidence}))
    return 0


# --- verification and split ----------------------------------------------------------------------

def remove_fixture(folder):
    """Remove our snapshot, including Git's read-only object files on Windows."""
    def make_writable_and_retry(fn, path, exc):
        if not isinstance(exc[1], PermissionError):
            raise exc[1]
        os.chmod(path, os.stat(path).st_mode | stat.S_IWUSR)
        fn(path)
    shutil.rmtree(folder, onerror=make_writable_and_retry)


def verify_inventory(args):
    """Each check against pristine snapshots, with no model inference: it must exit 1 at the parent
    and 0 at the last fix. A check that reports it cannot tell here (exit 3) is listed as such."""
    tasks = tasks_dir(args.tasks)
    source = Path(args.source).resolve()
    items = load_inventory(tasks, source)
    if args.ids:
        wanted = [i.strip().upper() for i in args.ids.split(',') if i.strip()]
        items = [by_id(items, i) for i in wanted]
    work = outside_repo(args.work, 'the work folder')
    report = []
    for t in items:
        entry = {'task': t['id'], 'runtime': t['runtime']}
        for side in ('base', 'fix'):
            folder = work / f'{t["id"]}-{side}'
            if folder.exists():
                remove_fixture(folder)
            fixture = build_fixture(source, side_commit(t, side), folder)
            started = time.monotonic()
            code, evidence = run_check(tasks / t['check'], fixture['dir'], work / f'{t["id"]}-{side}.check.txt')
            entry[side] = {'exit': code, 'evidence': evidence, 'seconds': round(time.monotonic() - started, 1)}
            if not args.keep:
                remove_fixture(folder)
        if 3 in (entry['base']['exit'], entry['fix']['exit']):
            entry['verdict'] = 'unavailable here'
        else:
            entry['verdict'] = 'discriminates' if (entry['base']['exit'], entry['fix']['exit']) == (1, 0) else 'BROKEN'
        report.append(entry)
        print(json.dumps(entry), flush=True)
    if args.report:
        outside_repo(args.report, 'the report').write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
    return 1 if any(e['verdict'] == 'BROKEN' for e in report) else 0


def plan_split(args):
    """Dev and validation by date and cluster: a cluster's date is its latest task's, a cluster goes
    wholly to one side, dev takes those up to --dev-until. Nothing here reads any run, so the split
    is made before any feedback exists."""
    items = load_inventory(tasks_dir(args.tasks), need_checks=False)
    date.fromisoformat(args.dev_until)
    cluster_date = {}
    for t in items:
        cluster_date[t['cluster']] = max(cluster_date.get(t['cluster'], ''), t['date'])
    side = {c: ('dev' if d <= args.dev_until else 'validation') for c, d in cluster_date.items()}
    split = {'dev_until': args.dev_until,
             'dev_clusters': sorted(c for c in side if side[c] == 'dev'),
             'validation_clusters': sorted(c for c in side if side[c] == 'validation'),
             'tasks': {t['id']: side[t['cluster']] for t in items}}
    body = json.dumps(split, sort_keys=True, indent=2) + '\n'
    if args.out:
        outside_repo(args.out, 'the split').write_text(body, encoding='utf-8')
    else:
        print(body, end='')
    return 0


def show_inventory(args):
    items = load_inventory(tasks_dir(args.tasks), Path(args.source).resolve())
    for t in items:
        print(f'{t["id"]}  {t["date"]}  {t["runtime"]:<8} {t["cluster"]:<30} {t["title"]}')
    return 0


def show_fixture(args):
    items = load_inventory(tasks_dir(args.tasks), Path(args.source).resolve(), need_checks=False)
    task = by_id(items, args.task)
    out = outside_repo(args.out, 'the fixture')
    print(json.dumps(build_fixture(Path(args.source).resolve(), side_commit(task, args.side), out)))
    return 0


def show_totals(args):
    campaign = outside_repo(args.campaign, 'the campaign folder')
    entries = ledger_entries(campaign / 'ledger.jsonl')
    classes = {}
    for e in entries:
        if e['kind'] == 'run':
            classes[e['class']] = classes.get(e['class'], 0) + 1
    print(json.dumps({**totals_of(entries), 'classes': classes}, sort_keys=True))
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(prog='turnrun.py', description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--tasks', help='the private task inventory folder (or TURNRUN_TASKS)')
    parser.add_argument('--source', default=str(REPO), help='the repository the task commits are in (default: this one)')
    sub = parser.add_subparsers(dest='command', required=True)

    sub.add_parser('inventory', help='validate the inventory and list it').set_defaults(fn=show_inventory)

    p = sub.add_parser('fixture', help='rebuild one task environment')
    p.add_argument('task')
    p.add_argument('--side', choices=['base', 'fix'], default='base')
    p.add_argument('--out', required=True)
    p.set_defaults(fn=show_fixture)

    p = sub.add_parser('verify-inventory', help='each check fails at base and passes at fix')
    p.add_argument('--work', required=True, help='scratch folder for snapshots, outside the repository')
    p.add_argument('--ids', help='comma-separated task ids (default: all)')
    p.add_argument('--report', help='write the JSON report here')
    p.add_argument('--keep', action='store_true', help='keep the snapshots')
    p.set_defaults(fn=verify_inventory)

    p = sub.add_parser('run', help='one offline run: fixture, brief, turn, check, ledger')
    p.add_argument('task')
    p.add_argument('--campaign', required=True, help='campaign folder (ledger and runs), outside the repository')
    p.add_argument('--agent', required=True, help='the shell agent the turn goes to (claude, codex, ...)')
    p.add_argument('--program', required=True, help='the agent program: an offline fake agent')
    p.add_argument('--budget-tokens', type=int, required=True, help='this run\'s cap, cache reads included')
    p.add_argument('--budget-seconds', type=int, required=True, help='this run\'s wall cap')
    p.add_argument('--total-budget-tokens', type=int, help='the campaign\'s cap, warm-ups and reservations included')
    p.add_argument('--rep', default='1', help='repetition key; a key runs once')
    p.add_argument('--warmup', action='store_true', help='a warm-up: keyed apart, counted in totals')
    p.add_argument('--mode', choices=['ask', 'auto', 'read'], default='auto',
                   help='the shell mode for the turn (default auto: a bug-fix turn edits and runs commands)')
    p.add_argument('--retry-interrupted', action='store_true', help='abandon an unfinished attempt and run again, once')
    p.add_argument('--no-check', action='store_true')
    p.add_argument('--binary', help=f'the runner (default {DEFAULT_BINARY})')
    p.add_argument('--policy', help='explicit restricted experimental policy JSON; pinned for this campaign')
    p.set_defaults(fn=run_task)

    p = sub.add_parser('split', help='dev/validation by date and cluster')
    p.add_argument('--dev-until', required=True)
    p.add_argument('--out')
    p.set_defaults(fn=plan_split)

    p = sub.add_parser('totals', help='recount a campaign\'s ledger')
    p.add_argument('campaign')
    p.set_defaults(fn=show_totals)

    args = parser.parse_args(argv)
    try:
        return args.fn(args)
    except (ValueError, RuntimeError, OSError, subprocess.SubprocessError) as exc:
        print(f'turnrun: {exc}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
