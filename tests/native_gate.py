"""Differential gate for the native runtime: run the same review, design and work tasks through this checkout's Python
runtime and a Rust `ringo` binary, with fake `codex`, `claude` and plain programs that replay canned replies, and
compare what each model call received and every record each run wrote. No model calls.

    python3 tests/native_gate.py --binary cli/target/release/ringo --out gate.json [--traces DIR ...]

The report names the binary and the Python runtime it was compared with; `ringo.py release stage --native BINARY
--native-gate gate.json` refuses a binary whose report shows a mismatch. --traces replays recorded runs (a review or
design trace folder: material.txt plus steps/*.txt replies) through both runtimes as extra tasks.

Compared exactly: every request (argv, stdin, the system prompt file, the private Codex home's files and modes, the
environment variables that decide auth), exit codes, stdout, and every file the run wrote. Normalised, and only these:
wall-clock fields (started, updated, seconds, elapsed_s), pid, the run's own temporary paths, and the implementation
identity (which runtime ran is the one intended difference).
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile

HERE = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(HERE))
import releases  # noqa: E402
from latency import sha  # noqa: E402

TIMES = {'started', 'updated', 'seconds', 'elapsed_s', 'pid'}
IDENTITY = {'mode', 'id', 'root', 'checkout_root', 'bootstrap_sha256', 'runtime', 'binary_sha256'}
AUTH_ENV = ('HOME', 'CODEX_HOME', 'USERPROFILE', 'RINGO_CHILD', 'OPENAI_API_KEY', 'ANTHROPIC_API_KEY',
            'OPENAI_BASE_URL', 'ANTHROPIC_AUTH_TOKEN', 'TYPESAFE_API_KEY', 'GATE_SECRET_API_KEY')

# One program plays codex, claude and a plain stdin command, by the name it is started as. It records the request,
# then prints the next canned reply in the form that program prints.
FAKE = r'''#!/usr/bin/env python3
import json, os, stat, sys
from pathlib import Path
record = Path(os.environ['GATE_RECORD'])
name = Path(sys.argv[0]).name
args = sys.argv[1:]
if name == 'codex' and args[:2] == ['debug', 'models']:
    print(json.dumps({'models': [{'slug': 'gate-model', 'tool_mode': 'x', 'apply_patch_tool_type': 'y',
                                  'experimental_supported_tools': ['shell']}]}))
    sys.exit(0)
stdin = sys.stdin.buffer.read().decode('utf-8', 'replace')
n = len(list(record.glob('call-*.json'))) + 1
replies = json.loads((record.parent / 'replies.json').read_text(encoding='utf-8'))
reply = replies[n - 1] if n <= len(replies) else 'No defects found.'
home = os.environ.get('CODEX_HOME', '')
private = {}
if name == 'codex' and home and Path(home).is_dir():
    for p in sorted(Path(home).rglob('*')):
        if p.is_file():
            private[p.relative_to(home).as_posix()] = [oct(stat.S_IMODE(p.stat().st_mode)), p.read_text('utf-8', 'replace')]
system = None
if '--system-prompt-file' in args:
    system = Path(args[args.index('--system-prompt-file') + 1]).read_text(encoding='utf-8')
env = {k: os.environ.get(k) for k in json.loads(os.environ['GATE_AUTH_ENV'])}
(record / f'call-{n:03}.json').write_text(json.dumps({'program': name, 'argv': args, 'stdin': stdin, 'system': system,
    'env': env, 'cwd_empty': not any(Path.cwd().iterdir()), 'private_home': private}, indent=1), encoding='utf-8')
if isinstance(reply, dict):  # a scripted failure
    sys.stdout.write(reply.get('stdout', ''))
    sys.stderr.write(reply.get('stderr', ''))
    sys.exit(reply.get('exit', 1))
if name == 'codex':
    for event in ({'type': 'thread.started', 'thread_id': 't'}, {'type': 'turn.started'},
                  {'type': 'item.completed', 'item': {'id': 'i0', 'type': 'reasoning', 'text': 'thinking'}},
                  {'type': 'item.completed', 'item': {'id': 'i1', 'type': 'agent_message', 'text': reply}},
                  {'type': 'turn.completed', 'usage': {'input_tokens': 1000 + len(stdin), 'cached_input_tokens': 0,
                                                       'output_tokens': 10 + len(reply)}}):
        print(json.dumps(event))
elif name == 'claude':
    print(json.dumps({'type': 'result', 'is_error': False, 'result': reply, 'total_cost_usd': 0.0125,
                      'usage': {'input_tokens': len(stdin), 'cache_creation_input_tokens': 0,
                                'cache_read_input_tokens': 7, 'output_tokens': len(reply)},
                      'modelUsage': {'claude-gate': {}}}))
else:
    sys.stdout.write(reply)
'''

FINDINGS = '''### 1. Off-by-one in the window (`context.py:366`)
- Code: "start = max(0, line - WINDOW)"
- Problem: the window starts one line late.
- Fix: subtract one more.

### 2. Missing check (`harness.py:12`)
- Problem: nothing checks the value.
- Fix: check it.

### Open questions
- Does a caller rely on the old order?

### Not checked
- Windows.
'''

SLUG_OK = '''@@@ RATIONALE
Lowercase, collapse non-alphanumerics, trim, cut to 40, fall back to n-a.
@@@ FILE slug.py
import re


def slugify(text):
    if not isinstance(text, str):
        raise TypeError("text must be a str")
    slug = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")[:40].rstrip("-")
    return slug or "n-a"
@@@ END
'''
SLUG_EDIT = '''@@@ REPLACE slug.py
[:41]
@@@ WITH
[:40]
@@@ END
'''
SLUG_BAD = SLUG_OK.replace('[:40]', '[:41]')


def tasks(source, gate_repo):
    """(name, argv, replies, files) for the built-in tasks; {DIR} in argv is the task's own input folder."""
    spec = json.loads((source / 'examples/v3-codex.json').read_text(encoding='utf-8'))
    spec_claude = {**spec, 'writer': 'claude', 'judge': 'claude', 'roles': {}}
    material = 'Review this change.\n\n--- a/x.py\n+++ b/x.py\n@@ -1 +1 @@\n-x = 1\n+x = 2  # é ✓\n'
    head = gate_repo['head']
    return [
        ('review-plain-no-defects', ['review', '--input', '{DIR}/material.txt', '--backend', '{FAKE_PLAIN}'],
         ['No defects found.'], {'material.txt': material}),
        ('review-plain-findings', ['review', '--input', '{DIR}/material.txt', '--backend', '{FAKE_PLAIN}'],
         [FINDINGS], {'material.txt': material}),
        ('review-codex-chatgpt-findings', ['review', '--input', '{DIR}/material.txt', '--backend', 'codex-chatgpt'],
         [FINDINGS], {'material.txt': material.replace('\n', '\r\n')}),
        ('review-claude', ['review', '--input', '{DIR}/material.txt', '--backend', 'claude'],
         [FINDINGS], {'material.txt': material}),
        ('review-repo', ['review', '--repo', '{REPO}', '--base', gate_repo['base'], '--head', head,
                         '--pr-text', '{DIR}/pr.txt', '--backend', 'codex-chatgpt',
                         '--findings-json', '{OUT}/findings.json'],
         [FINDINGS], {'pr.txt': 'Gate change: ünïcode, CRLF\r\nand a second line.\n'}),
        ('review-repo-small-budget', ['review', '--repo', '{REPO}', '--base', gate_repo['root'], '--head', head,
                                      '--context-budget', '6000', '--backend', '{FAKE_PLAIN}'],
         ['No defects found.'], {}),
        ('design-codex', ['design', '--input', '{DIR}/brief.md', '--backend', 'codex-chatgpt'],
         ['Decision\n\nDo the small thing first.\n'], {'brief.md': '# Brief\n\nDesign a cache. Budget: none.\n'}),
        ('review-codex-malformed-then-ok', ['review', '--input', '{DIR}/material.txt', '--backend', 'codex-chatgpt'],
         [{'stdout': 'not json\n', 'exit': 0}, FINDINGS], {'material.txt': material}),
        ('review-codex-malformed-twice', ['review', '--input', '{DIR}/material.txt', '--backend', 'codex-chatgpt'],
         [{'stdout': 'not json\n', 'exit': 0}] * 2, {'material.txt': material}),
        ('review-codex-exit-1-twice', ['review', '--input', '{DIR}/material.txt', '--backend', 'codex-chatgpt'],
         [{'stdout': '', 'stderr': 'boom\n  second line', 'exit': 1}] * 2, {'material.txt': material}),
        ('review-claude-error', ['review', '--input', '{DIR}/material.txt', '--backend', 'claude'],
         [{'stdout': '{"is_error": true, "result": "overloaded", "total_cost_usd": 0.5}', 'exit': 1}] * 2,
         {'material.txt': material}),
        ('review-max-cost', ['review', '--input', '{DIR}/material.txt', '--backend', 'claude', '--max-cost', '0.01',
                             '--unpriced-call-usd', '0.02'], [FINDINGS], {'material.txt': material}),
        ('review-bad-backend', ['review', '--input', '{DIR}/material.txt', '--backend', 'codex:fake-cli exec'],
         [], {'material.txt': material}),
        ('work-codex-first-try', ['work', '--spec', '{DIR}/spec.json', '--out', '{OUT}/work'],
         [SLUG_OK, 'No defects found.'], {'spec.json': json.dumps(spec)}),
        ('work-codex-revision', ['work', '--spec', '{DIR}/spec.json', '--out', '{OUT}/work'],
         [SLUG_BAD, SLUG_EDIT, 'No defects found.'], {'spec.json': json.dumps(spec)}),
        ('work-claude-protocol-repair', ['work', '--spec', '{DIR}/spec.json', '--out', '{OUT}/work'],
         ['I would rewrite slugify.', SLUG_OK, 'No defects found.'], {'spec.json': json.dumps(spec_claude)}),
    ]


def recorded(trace):
    """A task replaying a recorded review/design trace: its material and its replies, in call order."""
    trace = Path(trace)
    kind = json.loads((trace / 'plan.json').read_text(encoding='utf-8'))['kind']
    steps = sorted((trace / 'steps').glob('*.txt'))
    replies = [p.read_text(encoding='utf-8') for p in steps]
    return (f'trace-{trace.name}', [kind, '--input', '{DIR}/material.txt', '--backend', 'codex-chatgpt'],
            replies, {'material.txt': (trace / 'material.txt').read_text(encoding='utf-8')})


def gate_repository(root):
    """A small git repository with a few commits whose context exercises definitions, references and tests."""
    repo = root / 'repo'
    repo.mkdir()
    run = lambda *a: subprocess.run(['git', '-C', str(repo), *a], check=True, capture_output=True, text=True).stdout.strip()
    run('init', '-q', '-b', 'main')
    run('config', 'user.email', 'gate@example.invalid')
    run('config', 'user.name', 'gate')
    files = {
        'README.md': '# Gate\n\nA fixture.\n',
        'pkg/core.py': 'LIMIT = 10\n\n\nclass Base:\n    def size(self):\n        return LIMIT\n\n\n'
                       'def helper(x):\n    return x + 1\n',
        'pkg/use.py': 'from pkg.core import Base, helper\n\n\nclass Child(Base):\n    pass\n\n\n'
                      'def run():\n    return helper(Child().size())\n',
        'tests/test_core.py': 'from pkg.core import helper\n\n\ndef test_helper():\n    assert helper(1) == 2\n',
        'src/lib.rs': 'pub fn area(w: u32, h: u32) -> u32 {\n    w * h\n}\n',
    }
    for name, text in files.items():
        (repo / name).parent.mkdir(parents=True, exist_ok=True)
        (repo / name).write_text(text, encoding='utf-8')
    run('add', '-A')
    run('commit', '-qm', 'one')
    first = run('rev-parse', 'HEAD')
    (repo / 'pkg/core.py').write_text(files['pkg/core.py'].replace('LIMIT = 10', 'LIMIT = 12')
                                      .replace('return x + 1', 'return x + 2'), encoding='utf-8')
    run('commit', '-qam', 'two')
    base = run('rev-parse', 'HEAD')
    (repo / 'src/lib.rs').write_text('pub fn area(w: u64, h: u64) -> u64 {\n    w.saturating_mul(h)\n}\n', encoding='utf-8')
    (repo / 'pkg/use.py').write_text(files['pkg/use.py'] + '\n\ndef twice():\n    return run() * 2\n', encoding='utf-8')
    run('commit', '-qam', 'three: ünïcode')
    return {'path': repo, 'root': first, 'base': base, 'head': run('rev-parse', 'HEAD')}


def normalise(value, run_root):
    if isinstance(value, dict):
        return {k: ('<identity>' if k in IDENTITY else normalise(v, run_root)) for k, v in value.items() if k not in TIMES}
    if isinstance(value, list):
        return [normalise(v, run_root) for v in value]
    if isinstance(value, str):
        return value.replace(str(run_root), '<RUN>')
    return value


def snapshot(run_root, tmp_prefixes):
    """Every file the run wrote, and every request the fakes recorded, normalised as the module docstring says."""
    out = {}
    for path in sorted(run_root.rglob('*')):
        if not path.is_file() or path.name in ('replies.json',):
            continue
        rel = path.relative_to(run_root).as_posix()
        data = path.read_bytes()
        try:
            text = data.decode('utf-8')
        except UnicodeDecodeError:
            out[rel] = 'sha256:' + hashlib.sha256(data).hexdigest()
            continue
        if path.suffix in ('.json', '.jsonl'):
            try:
                records = [json.loads(line) for line in text.splitlines() if line.strip()] if path.suffix == '.jsonl' \
                    else json.loads(text)
            except ValueError:
                out[rel] = text
                continue
            normalised = normalise(records, run_root)
            text = json.dumps(normalised, indent=1, ensure_ascii=False)
            for prefix in tmp_prefixes:  # private per-call temporary folders differ by name only
                import re
                text = re.sub(re.escape(prefix) + r'[^"/\\]*', prefix + '<tmp>', text)
        else:
            text = text.replace(str(run_root), '<RUN>')
        out[rel] = text
    return out


def run_task(task, arm, source, binary, fakes, gate_repo, work):
    name, argv, replies, files = task
    root = work / name / 'run'  # both arms run at the same path, so paths inside requests match; then it moves
    (root / 'record').mkdir(parents=True)
    (root / 'in').mkdir()
    (root / 'out').mkdir()
    (root / 'replies.json').write_text(json.dumps(replies), encoding='utf-8')
    for rel, text in files.items():
        (root / 'in' / rel).write_bytes(text.encode('utf-8'))
    home = root / 'owner-home'
    (home / '.codex').mkdir(parents=True)
    (home / '.codex/auth.json').write_text('{"auth_mode": "chatgpt", "tokens": "gate-fake"}\n', encoding='utf-8')
    (home / '.codex/models_cache.json').write_text('{"models": []}\n', encoding='utf-8')
    subs = {'{DIR}': str(root / 'in'), '{OUT}': str(root / 'out'), '{REPO}': str(gate_repo['path']),
            '{FAKE_PLAIN}': shlex.join([str(fakes / 'plain')])}
    args = [arg for arg in argv]
    for k, v in subs.items():
        args = [a.replace(k, v) for a in args]
    if args[0] in ('review', 'design'):
        args += ['--trace', str(root / 'trace'), '--out', str(root / 'out/result.md'), '--timeout', '30']
    state = root / 'release-state'  # empty: no active release, so both run as checkout code
    env = {k: v for k, v in os.environ.items() if k not in ('RINGO_CHILD', 'RINGO_PINNED', 'CODEX_HOME')}
    env.update(PATH=f'{fakes}{os.pathsep}{env.get("PATH", "")}', HOME=str(home), RINGO_RELEASE_HOME=str(state),
               RINGO_HOME=str(source), RINGO_PYTHON=sys.executable, GATE_RECORD=str(root / 'record'),
               GATE_AUTH_ENV=json.dumps(AUTH_ENV), OPENAI_API_KEY='gate-must-be-stripped',
               GATE_SECRET_API_KEY='gate-must-be-stripped', PYTHONDONTWRITEBYTECODE='1', LC_ALL='C.UTF-8', TZ='UTC')
    command = [sys.executable, str(source / 'ringo.py'), *args] if arm == 'python' else [str(binary), *args]
    proc = subprocess.run(command, cwd=root / 'in', env=env, capture_output=True, timeout=300)
    result = snapshot(root, ['council-judge-', 'ringo-system-'])
    result['<exit>'] = str(proc.returncode)
    for stream, data in (('<stdout>', proc.stdout), ('<stderr>', proc.stderr)):
        text = data.decode('utf-8', 'replace').replace(str(root), '<RUN>')
        lines = []
        for line in text.splitlines():  # the harness summary line carries timings and the runtime identity
            if line.startswith('harness: {'):
                try:
                    line = 'harness: ' + json.dumps(normalise(json.loads(line[9:]), root), ensure_ascii=False)
                except ValueError:
                    pass
            lines.append(line)
        result[stream] = '\n'.join(lines)
    root.rename(root.parent / arm)
    return result


def compare(name, py, native):
    mismatches = []
    for key in sorted(set(py) | set(native)):
        if key not in py or key not in native:
            mismatches.append(f'{name}: {key} only in the {"python" if key in py else "native"} run')
        elif py[key] != native[key]:
            a, b = py[key], native[key]
            at = next((i for i, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
            mismatches.append(f'{name}: {key} differs at character {at}: python {a[max(0, at - 60):at + 60]!r} '
                              f'native {b[max(0, at - 60):at + 60]!r}')
    return mismatches


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('--binary', required=True, type=Path, help='the native `ringo` binary to compare')
    parser.add_argument('--out', type=Path, help='write the JSON report here (default: stdout)')
    parser.add_argument('--traces', nargs='*', default=[], help='recorded review/design trace folders to replay too')
    parser.add_argument('--keep', type=Path, help='keep the runs in this new folder for inspection')
    args = parser.parse_args(argv)
    binary = args.binary.resolve()
    if os.name == 'nt':
        parser.error('the native runtime runs on macOS and Linux')
    with tempfile.TemporaryDirectory(prefix='ringo-gate-') as tmp:
        work = args.keep.resolve() if args.keep else Path(tmp)
        work.mkdir(parents=True, exist_ok=True)
        fakes = work / 'fakes'
        fakes.mkdir()
        for program in ('codex', 'claude', 'plain'):
            (fakes / program).write_text(FAKE, encoding='utf-8')
            (fakes / program).chmod(0o755)
        gate_repo = gate_repository(work)
        all_tasks = tasks(HERE, gate_repo) + [recorded(t) for t in args.traces]
        names, mismatches = [], []
        for task in all_tasks:
            names.append(task[0])
            py = run_task(task, 'python', HERE, binary, fakes, gate_repo, work)
            native = run_task(task, 'native', HERE, binary, fakes, gate_repo, work)
            found = compare(task[0], py, native)
            mismatches += found
            print(f'{task[0]}: {"identical" if not found else f"{len(found)} mismatches"}', file=sys.stderr)
        report = {'version': 1, 'binary_sha256': releases.file_hash(binary),
                  'reference_sha256': sha(releases.inventory(HERE)), 'tasks': names, 'mismatches': mismatches}
    text = json.dumps(report, indent=1)
    if args.out:
        args.out.write_text(text + '\n', encoding='utf-8')
    else:
        print(text)
    return 0 if not mismatches else 1


if __name__ == '__main__':
    sys.exit(main())
