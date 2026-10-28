"""Paired local CLI latency experiments. Simulated backends only; no provider calls.

Keep baseline checkout, raw measurements and reports outside the project.
active-fixture mode creates a labeled synthetic approval ONLY in the new private
experiment store, to time approval loading and real dispatch. It never promotes
anything in the normal user store. Preview is separately available.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import random
import shlex
import shutil
import subprocess
import sys
import time

import latency
import backends
import releases


def baseline_setup(baseline=None, release_id=None, state=None, launcher=None):
    if (baseline is None) == (release_id is None):
        raise ValueError('choose exactly one baseline checkout or baseline release')
    if release_id:
        if state is None:
            raise ValueError('a baseline release needs its state directory')
        state = releases.state_root(state)
        bundle, manifest = releases.verify(state, release_id)
        entry = Path(launcher).resolve() if launcher else releases.HERE / 'ringo.py'
        if not entry.is_file():
            raise ValueError('baseline launcher must be an existing Python entrypoint')
        prefix = [sys.executable, str(entry), 'release', '--state', str(state),
                  'preview', '--release', release_id, '--']
        return release_id, manifest['default_review_sha256'], prefix
    if launcher is not None:
        raise ValueError('a custom baseline launcher requires --baseline-release')
    baseline = Path(baseline).resolve()
    identity = 'checkout:' + latency.sha({name: releases.file_hash(baseline / name)
                                        for name in releases.RUNTIME if (baseline / name).is_file()})
    return identity, releases.config_hash(releases.default_plan(baseline)), \
        [sys.executable, str(baseline / 'ringo.py')]


def selection_fixture(state, rid, count):
    """Test setup, not performance evidence or a human adoption decision.

    Use constant synthetic samples only to construct a realistic-sized approval
    file for timing the active read/verify path. Keep it in the experiment's state
    and identify the approver explicitly as a fixture. Actual observations are
    collected afterwards and never used to activate a normal user release.
    """
    _, manifest = releases.verify(state, rid)
    reports = []
    for workload in sorted(releases.LATENCY_WORKLOADS):
        synthetic = {'version':1,'baseline':'synthetic-test-setup','candidate':rid,'planned_pairs':count,
            'protocol':{'workload':workload,'environment':'SYNTHETIC APPROVAL FIXTURE, NOT MEASURED',
                'baseline_config':'fixture','candidate_config':manifest['default_review_sha256'],
                'order':'alternating','warmups':0,'clock':'perf_counter_ns','kind':'simulated'},
            'pairs':[{'id':str(i),'baseline_s':1,'candidate_s':1,'baseline_ok':True,'candidate_ok':True}
                     for i in range(count)]}
        reports.append(latency.assess(synthetic, {'absolute_ms':5,'relative':.05,
            'min_pairs':30,'resamples':500,'seed':1}))
    return releases.activate(state, rid, {'version':1,'reports':reports},
                             'OFFLINE TEST FIXTURE — NOT HUMAN ADOPTION', 'none')


def run(baseline, out, pairs=100, warmups=5, absolute_ms=5, relative=.05, workloads=None,
        baseline_release=None, baseline_state=None, execution='preview', baseline_launcher=None):
    baseline_id, baseline_config, baseline_prefix = baseline_setup(
        baseline, baseline_release, baseline_state, baseline_launcher)
    out = Path(out).resolve()
    import harness
    harness.outside_repo(out, 'latency experiment')
    if pairs < 30 or warmups < 0:
        raise ValueError('at least 30 pairs and nonnegative warmups required')
    if execution not in ('preview','active-fixture'):
        raise ValueError('execution must be preview or active-fixture')
    if baseline_release and execution == 'active-fixture':
        base_state = releases.state_root(baseline_state)
        if not releases.pointer(base_state)['active'] \
                or releases.pointer(base_state)['active']['release'] != baseline_release:
            raise ValueError('active-path baseline must match its state pointer')
        # Bootstrap changes must use the before-version launcher too; selecting
        # the old bundle through the new launcher would time the new verifier.
        baseline_prefix = baseline_prefix[:2]
    workloads = workloads or ['startup', 'review', 'review-trace', 'review-large', 'design', 'work',
                             'review-delay', 'review-override', 'review-failure']
    known = releases.LATENCY_WORKLOADS | {'review-delay', 'review-override', 'review-failure'}
    if len(set(workloads)) != len(workloads) or set(workloads) - known:
        raise ValueError('select unique known workloads')
    backends.private_directory(out, create=True, mode=0o700, parents=True, exist_ok=False)
    fake = out / 'backend.py'
    fake.write_text('''import json, os, sys, time
text = sys.stdin.read()
time.sleep(float(os.environ.get('RINGO_FAKE_DELAY', '0')))
if os.environ.get('RINGO_FAKE_FAIL') == '1':
    sys.exit(1)
if text.startswith('Implement the task'):
    print(json.dumps({'files': {'value.py': 'def f(x): return 2*x\\n'}, 'rationale': 'fixture'}))
elif text.startswith('You are an independent council judge'):
    print(json.dumps({'accept': True}))
elif os.environ.get('RINGO_FAKE_DESIGN') == '1':
    print('A simulated design proposal. Use two workers; validate against the fixed constraints.')
else:
    print('No defects found.')
''')
    bridge = out / 'bridge.py'
    bridge.write_text('''import json,sys
r=json.load(sys.stdin)
if r['operation']=='prepare':
    result={'fingerprint':'fixture','policy_sha256':'a'*64,'requests':['JUDGE']*r['samples']}
else:
    result={'report':{'complete':True,'scored':[],'decision':{'action':'recommend'}}}
print(json.dumps({'version':1, **result}))
''')
    small, large = out / 'small.txt', out / 'large.txt'
    small.write_text('A fixed independent fixture.\n' * 100)
    large.write_text('A fixed independent fixture with unchanged context.\n' * 8000)
    backend = shlex.join([sys.executable, str(fake)])
    spec = {'version': 1, 'brief': 'Implement twice.\n\nHard constraints:\n- f(5) returns 10.\n',
            'files': {'value.py': ''}, 'fixtures': {},
            'checks': [{'name': 'fixed fixture', 'argv': [sys.executable, '-c',
                'import runpy; f=runpy.run_path("value.py")["f"]; raise SystemExit(0 if f(5)==10 else 1)']}],
            'allow_execution': True, 'allow_tool_backends': True, 'writer': backend, 'judge': backend,
            'council': [sys.executable, str(bridge)], 'rubric': str(out / 'fixture-rubric.json'),
            'samples': 6, 'max_rounds': 1, 'max_seconds': 30, 'timeout_s': 10}
    task = out / 'task.json'
    task.write_text(json.dumps(spec))
    override = out / 'override.json'
    override.write_text(json.dumps({'version': 1, 'kind': 'review', 'role': 'Review fixture.',
        'prompts_dir': str(releases.HERE / 'prompts'),
        'passes': [{'id': 'review', 'prompt': 'review.md', 'reply': 'findings'}]}))
    state = out / 'state'
    rid = releases.stage(releases.HERE, state)
    _, manifest = releases.verify(state, rid)
    base_state = None
    if execution == 'active-fixture':
        selection_fixture(state,rid,pairs)
        if baseline_release:
            # Time code, not approval size: every launch parses and hashes the active approval,
            # and a real one embeds its whole latency suite. Give both arms the same synthetic
            # approval by mirroring the verified baseline bundle into private experiment state.
            bundle, _ = releases.verify(releases.state_root(baseline_state), baseline_release)
            base_state = releases.private_root(out / 'baseline-state')
            destination = base_state / 'bundles' / baseline_release
            if os.name == 'nt':
                backends.private_directory(destination, create=True)
                for directory in bundle.rglob('*'):
                    if directory.is_dir():
                        backends.private_directory(destination / directory.relative_to(bundle), create=True,
                                                   parents=True, exist_ok=True)
                shutil.copytree(bundle, destination, dirs_exist_ok=True)
            else:
                shutil.copytree(bundle, destination)
            selection_fixture(base_state, baseline_release, pairs)
    env = {**os.environ, 'RINGO_RELEASE_HOME': str(out / 'no-active')}
    rng, reports = random.Random(42), []

    def invoke(workload, arm, label):
        local_env = dict(env, RINGO_FAKE_DELAY='.05' if workload == 'review-delay' else '0',
                         RINGO_FAKE_DESIGN='1' if workload == 'design' else '0',
                         RINGO_FAKE_FAIL='1' if workload == 'review-failure' else '0')
        if execution == 'active-fixture':
            if arm == 'candidate':
                local_env['RINGO_RELEASE_HOME'] = str(state)
            elif baseline_release:
                local_env['RINGO_RELEASE_HOME'] = str(base_state)
        if workload == 'startup':
            args = ['--help']
        elif workload == 'work':
            args = ['work', '--spec', str(task), '--out', str(out / (label + '-work'))]
        else:
            args = ['design' if workload == 'design' else 'review', '--input',
                    str(large if workload == 'review-large' else small), '--backend', backend]
            if workload == 'review-trace':
                args += ['--trace', str(out / (label + '-trace'))]
            if workload == 'review-override':
                args += ['--pipeline', str(override)]
        candidate_prefix = [sys.executable,str(releases.HERE/'ringo.py')] if execution == 'active-fixture' else \
            [sys.executable,str(releases.HERE/'ringo.py'),'release','--state',str(state),
             'preview','--release',rid,'--']
        command = [*(baseline_prefix if arm == 'baseline' else candidate_prefix),*args]
        started = time.perf_counter_ns()
        try:
            proc = subprocess.run(command, env=local_env, capture_output=True, timeout=45)
            elapsed = (time.perf_counter_ns() - started) / 1e9
            # A deliberately failing backend must remain a failed observation; it is
            # useful diagnostic coverage, not promotion evidence of successful work.
            ok, code = proc.returncode == 0, proc.returncode
            (out / (label + '.stderr')).write_bytes(proc.stderr)
        except subprocess.TimeoutExpired:
            elapsed, ok, code = (time.perf_counter_ns() - started) / 1e9, False, 'timeout'
        with (out / 'execution.jsonl').open('a') as f:
            f.write(json.dumps({'label': label, 'arm': arm, 'command': command,
                                'seconds': elapsed, 'ok': ok, 'exit': code}) + '\n')
        return elapsed, ok

    for workload in workloads:
        rows = []
        for index in range(-warmups, pairs):
            order = ['baseline', 'candidate']
            rng.shuffle(order)
            observed = {arm: invoke(workload, arm, f'{workload}-{index}-{arm}') for arm in order}
            if index >= 0:
                rows.append({'id': str(index), 'baseline_s': observed['baseline'][0],
                    'candidate_s': observed['candidate'][0], 'baseline_ok': observed['baseline'][1],
                    'candidate_ok': observed['candidate'][1]})
        measurement = {'version': 1, 'baseline': baseline_id, 'candidate': rid, 'planned_pairs': pairs,
            'protocol': {'workload': workload, 'environment': platform.platform() + ' Python ' + platform.python_version()
                         + ' selection=' + execution,
                         'baseline_config': baseline_config, 'candidate_config': manifest['default_review_sha256'],
                         'order': 'randomized', 'warmups': warmups, 'clock': 'perf_counter_ns', 'kind': 'simulated'},
            'pairs': rows}
        report = latency.assess(measurement, {'absolute_ms': absolute_ms, 'relative': relative,
            'min_pairs': 100, 'resamples': 2000, 'seed': 7})
        reports.append(report)
        (out / (workload + '.json')).write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps({'workload': workload, 'status': report['status'], 'baseline_ms': report['baseline_ms'],
                          'candidate_ms': report['candidate_ms'], 'metrics': report['metrics']}, sort_keys=True), flush=True)
    suite = {'version': 1, 'reports': reports}
    (out / 'suite.json').write_text(json.dumps(suite, indent=2) + '\n')
    return suite


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    baselines = parser.add_mutually_exclusive_group(required=True)
    baselines.add_argument('--baseline', type=Path)
    baselines.add_argument('--baseline-release')
    parser.add_argument('--baseline-state', type=Path)
    parser.add_argument('--baseline-launcher', type=Path,
                        help='before-version ringo.py for comparing stable-bootstrap changes')
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--pairs', type=int, default=100)
    parser.add_argument('--warmups', type=int, default=5)
    parser.add_argument('--absolute-ms', type=float, default=5)
    parser.add_argument('--relative', type=float, default=.05)
    parser.add_argument('--workload', action='append', dest='workloads')
    parser.add_argument('--execution', choices=('preview','active-fixture'),default='preview')
    args = parser.parse_args(argv)
    try:
        run(args.baseline, args.out, args.pairs, args.warmups, args.absolute_ms, args.relative,
            args.workloads, args.baseline_release, args.baseline_state,args.execution,args.baseline_launcher)
        return 0
    except (ValueError, OSError) as exc:
        print(f'latency experiment: {exc}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
