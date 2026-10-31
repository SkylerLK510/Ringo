"""Soft time limits: a call past --timeout is checked and probed, not killed; the person may stop it (one retry);
the limiter stops it at twice the soft limit, never under HARD_FLOOR_S. Offline, with stand-in backends."""
import io
import json
import os
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

import backends
import harness
import pipelines as pl
import workflow
from tests.test_harness import REVIEW
from tests.test_workflow import FAKE_BRIDGE

# A plain stand-in backend: the probe gets OK at once; a review call sleeps $DELAY, except that with
# $HANG_FIRST its first invocation hangs. Invocations are counted in $COUNTER.
CHILD = r'''
import os, sys, time
prompt = sys.stdin.read()
if 'Reply with the single word OK.' in prompt:
    sys.exit(3) if os.environ.get('PROBE_FAILS') else print('OK')
    sys.exit(0)
path = os.environ['COUNTER']
n = int(open(path).read() or 0) + 1 if os.path.exists(path) else 1
open(path, 'w').write(str(n))
time.sleep(60 if os.environ.get('HANG_FIRST') and n == 1 else float(os.environ.get('DELAY', '0')))
sys.stdout.write(os.environ['REPLY'])
'''


class SoftLimitTest(unittest.TestCase):
    def setUp(self):
        d = tempfile.TemporaryDirectory()
        self.addCleanup(d.cleanup)
        self.root = Path(d.name)
        self.backend = {'name': 'stand-in', 'argv': [sys.executable, '-c', CHILD], 'style': 'plain'}
        self.env = {'COUNTER': str(self.root / 'count'), 'REPLY': REVIEW}
        self.err = io.StringIO()
        for patch in (mock.patch('sys.stderr', self.err), mock.patch.object(harness, 'PROBE_TIMEOUT_S', 10)):
            patch.start()
            self.addCleanup(patch.stop)

    def review(self, run, timeout_s, floor, **env):
        with mock.patch.dict(os.environ, {**self.env, **env}), mock.patch.object(harness, 'HARD_FLOOR_S', floor):
            return harness.run_plan(pl.load_plan(None, 'review'), 'M', self.backend, run, timeout_s=timeout_s)

    def rows(self, trace):
        return [json.loads(x) for x in (trace / 'trace.jsonl').read_text().splitlines()]

    def test_a_call_past_its_soft_limit_is_not_killed_and_is_probed_once(self):
        trace = self.root / 't'
        run = harness.Run(trace)
        text, _ = self.review(run, 0.3, 20, DELAY='1.2')
        self.assertIn('Bug one', text)
        rows = self.rows(trace)
        self.assertEqual([(r['step'], r['status']) for r in rows if r['status'] != 'started'],
                         [('review.1.probe', 'ok'), ('review', 'ok')])
        ok = next(r for r in rows if r['step'] == 'review' and r['status'] == 'ok')
        self.assertEqual((ok['soft_s'], ok['limit_s']), (0.3, 20))
        self.assertGreater(ok['seconds'], 1.0)
        incident = json.loads((trace / 'incidents' / 'review.1.json').read_text())
        self.assertEqual((incident['probe']['status'], incident['recommendation'], incident['decision']),
                         ('ok', 'wait', None))
        self.assertTrue(incident['checks']['running'])
        self.assertEqual((run.summary()['calls'], run.summary()['incidents']), (2, 1))
        self.assertIn('soft limit and is still running', self.err.getvalue())

    def test_a_stop_decision_stops_the_call_and_retries_it_once(self):
        trace = self.root / 't'
        run = harness.Run(trace)
        def decide():
            path = trace / 'incidents' / 'review.1.json'
            for _ in range(200):
                if path.exists() and json.loads(path.read_text()).get('recommendation'):
                    Path(json.loads(path.read_text())['decision_file']).write_text('stop\n')
                    return
                time.sleep(0.05)
        threading.Thread(target=decide, daemon=True).start()
        started = time.monotonic()
        text, _ = self.review(run, 0.3, 30, HANG_FIRST='1')
        self.assertLess(time.monotonic() - started, 20)  # stopped long before its 30s limit
        self.assertIn('Bug one', text)
        main = [(r['attempt'], r['status'], r.get('stopped')) for r in self.rows(trace)
                if r['step'] == 'review' and r['status'] != 'started']
        self.assertEqual(main, [(1, 'error', 'person'), (2, 'ok', None)])
        self.assertEqual(json.loads((trace / 'incidents' / 'review.1.json').read_text())['decision'], 'stop')

    def test_the_limiter_stops_a_call_at_twice_its_soft_limit_without_a_retry(self):
        trace = self.root / 't'
        with self.assertRaises(backends.subprocess.TimeoutExpired) as caught:
            self.review(harness.Run(trace), 0.4, 0, HANG_FIRST='1')
        self.assertTrue(caught.exception.limit)
        main = [r for r in self.rows(trace) if r['step'] == 'review' and r['status'] != 'started']
        self.assertEqual([(r['attempt'], r['stopped'], r['limit_s']) for r in main], [(1, 'limit', 0.8)])

    def watch_once(self, run, probe_status, elapsed, comparable=()):
        for seconds in comparable:
            run.finish('review', seconds, {'input': 10, 'cache_read': 0, 'cache_write': 0, 'output': 5})
        watch = harness.Watch(run, 'review', 1, self.backend, 1, 900, 'review',
                              lambda done: {'status': probe_status, 'seconds': 0.1})
        thread = threading.Thread(target=watch.soft, args=(elapsed, lambda: ('', '')))
        thread.start()
        for _ in range(200):
            if watch.incident and watch.incident.get('recommendation'):
                break
            time.sleep(0.02)
        watch.done.set()
        thread.join(5)
        return watch.incident

    def test_comparable_calls_that_finished_fast_suggest_a_stall_and_ask(self):
        incident = self.watch_once(harness.Run(self.root / 't'), 'ok', 40.0, comparable=(5.0, 9.0))
        self.assertEqual((incident['recommendation'], incident['checks']['comparable_seconds']), ('stop', [5.0, 9.0]))
        self.assertIn('15 tokens like comparable calls', incident['options']['stop'])
        self.assertIn('consider stopping it', self.err.getvalue())

    def test_no_comparable_evidence_or_a_dead_backend_means_keep_waiting(self):
        for k, (status, comparable) in enumerate((('ok', ()), ('ok', (30.0,)), ('error', (1.0,)))):
            with self.subTest(status=status, comparable=comparable):
                incident = self.watch_once(harness.Run(self.root / f't{k}'), status, 40.0, comparable)
                self.assertEqual(incident['recommendation'], 'wait')

    def test_an_unprobed_stall_is_reported_but_not_recommended_for_stopping(self):
        incident = self.watch_once(harness.Run(self.root / 't'), 'skipped', 40.0, comparable=(5.0,))
        self.assertEqual(incident['recommendation'], 'wait')
        self.assertIn('not probed', incident['diagnosis'])

    def test_protected_evaluation_wrappers_keep_hard_timeouts(self):
        def review_call(*args):  # evaluate.run's wrapper shape: it forwards no watch
            return args
        self.assertFalse(harness.accepts_watch(review_call))
        self.assertTrue(harness.accepts_watch(harness.call))
        self.assertTrue(harness.accepts_watch(lambda b, s, m, t, watch=None: None))
        self.assertFalse(harness.accepts_watch(lambda b, s, m, t: [b for watch in ()]))  # a local, not a parameter

    def test_harness_import_does_not_load_inspect(self):
        import subprocess
        code = 'import sys, harness; print("inspect" in sys.modules)'
        out = subprocess.run([sys.executable, '-c', code], cwd=Path(harness.__file__).parent,
                             capture_output=True, text=True, timeout=30)
        self.assertEqual(out.stdout.strip(), 'False', out.stderr)

    def test_the_call_waits_for_its_probe_and_counts_it(self):
        trace = self.root / 't'
        run = harness.Run(trace)
        slow_probe = CHILD.replace("print('OK')", "time.sleep(60); print('OK')")
        self.backend['argv'] = [sys.executable, '-c', slow_probe]
        text, _ = self.review(run, 0.3, 20, DELAY='0.8')
        self.assertIn('Bug one', text)
        probe = [(r['status']) for r in self.rows(trace) if r['step'] == 'review.1.probe']
        self.assertEqual(probe, ['started', 'error'])  # stopped with its call, recorded before the run returned
        self.assertEqual(run.summary()['calls'], 2)

    def test_setup_counts_against_the_limit(self):
        marker = self.root / 'launched'
        backend = {'name': 'codex-chatgpt', 'argv': ['sh', '-c', f'touch {marker}; sleep 5'], 'style': 'codex', 'lean': True}
        slow_catalog = lambda argv, private, env, timeout_s=15: (time.sleep(0.5), argv)[1]
        with mock.patch.object(backends, '_lean_codex', side_effect=slow_catalog), \
                mock.patch.dict(os.environ, {'HOME': str(self.root), 'CODEX_HOME': str(self.root / 'c')}):
            started = time.monotonic()
            with self.assertRaises(backends.subprocess.TimeoutExpired) as caught:
                backends.run_isolated(backend['argv'], 'x', 0.4, codex_home=True, lean=True)
        self.assertTrue(caught.exception.limit)
        self.assertLess(time.monotonic() - started, 2)
        self.assertFalse(marker.exists())  # setup used the window, so the model was never launched

    def test_no_probe_under_a_cost_cap(self):
        run = harness.Run(max_cost=1.0)
        self.assertEqual(harness.probe(run, self.backend, 'review.1')['status'], 'skipped')
        self.assertEqual(run.summary()['calls'], 0)

    def test_a_probe_is_stopped_when_its_call_ends(self):
        run, done = harness.Run(), threading.Event()
        threading.Timer(0.3, done.set).start()
        started = time.monotonic()
        with mock.patch.dict(os.environ, {**self.env, 'HANG_FIRST': '1'}):
            result = harness.probe(run, {**self.backend, 'argv': [sys.executable, '-c', 'import time; time.sleep(60)']},
                                   'review.1', stop=done)
        self.assertEqual(result['status'], 'error')
        self.assertLess(time.monotonic() - started, 5)

    def test_work_retries_a_call_the_person_stopped_once(self):
        (self.root / 'bridge.py').write_text(FAKE_BRIDGE)
        spec = {'version': 1, 'brief': 'Implement f(x)=2*x.\n\nHard constraints:\n- Return twice x.\n',
                'files': {'value.py': ''},
                'fixtures': {'check.py': "from pathlib import Path\nn={}\nexec(Path('value.py').read_text(), n)\nassert n['f'](5)==10\n"},
                'checks': [{'name': 'reference', 'argv': [sys.executable, '-I', '-B', 'check.py']}],
                'writer': 'fake', 'judge': 'fake', 'allow_tool_backends': True, 'allow_execution': True,
                'council': [sys.executable, str(self.root / 'bridge.py')], 'rubric': 'unused.json',
                'max_rounds': 1, 'max_seconds': 60, 'samples': 2}
        path = self.root / 'task.json'
        path.write_text(json.dumps(spec))
        stops = []

        def call(_backend, system, message, timeout_s, watch=None):
            self.assertIsInstance(watch, harness.Watch)
            if not system.startswith('Implement the task'):
                return json.dumps({'accept': True}), {'cost_usd': 0.01}
            if not stops:  # the first writer attempt is stopped by the person
                stops.append(watch.attempt)
                raise backends.Stopped(['writer'], timeout_s)
            return json.dumps({'files': {'value.py': 'def f(x): return 2*x\n'}, 'rationale': 'r'}), {'cost_usd': 0.01}

        report = workflow.run(path, self.root / 'run', call_fn=call)
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['usage']['calls'], 1 + 1 + 2)  # stopped writer, its retry, two judges
        writer = [json.loads(x) for x in (self.root / 'run/trace.jsonl').read_text().splitlines()
                  if json.loads(x)['step'] == 'writer-1' and json.loads(x)['status'] != 'reserved']
        self.assertEqual([(r['attempt'], r['status'], r.get('stopped')) for r in writer],
                         [(1, 'error', 'person'), (2, 'ok', None)])
        self.assertTrue(50 < writer[0]['soft_s'] <= writer[0]['limit_s'] <= 60)  # both capped by max_seconds


    def test_the_work_review_is_soft_too_and_capped_by_the_time_left(self):
        # The review goes through run_plan: it gets a watch (so a slow review is probed, not killed and sent again),
        # and its limits are capped by the run's time left, as the writer's are.
        spec = {'version': 1, 'brief': 'Implement f(x)=2*x.\n\nHard constraints:\n- Return twice x.\n',
                'files': {'value.py': ''},
                'fixtures': {'check.py': "from pathlib import Path\nn={}\nexec(Path('value.py').read_text(), n)\nassert n['f'](5)==10\n"},
                'checks': [{'name': 'reference', 'argv': [sys.executable, '-I', '-B', 'check.py']}],
                'writer': 'fake', 'judge': 'fake', 'allow_tool_backends': True, 'allow_execution': True,
                'max_rounds': 1, 'max_seconds': 60}
        path = self.root / 'task.json'
        path.write_text(json.dumps(spec))
        watches = []

        def call(_backend, system, message, timeout_s, watch=None):
            watches.append(watch)
            if system.startswith('Implement the task'):
                return json.dumps({'files': {'value.py': 'def f(x): return 2*x\n'}, 'rationale': 'r'}), {'cost_usd': 0.01}
            return 'What this change does: adds f.\n\nNo defects found.', {'cost_usd': 0.01}

        report = workflow.run(path, self.root / 'run', call_fn=call)
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual([type(w) for w in watches], [harness.Watch, harness.Watch])  # writer, review
        review = [r for r in self.rows(self.root / 'run') if r.get('pass') == 'review' and r['status'] == 'started']
        self.assertEqual(len(review), 1)
        self.assertTrue(50 < review[0]['soft_s'] <= review[0]['limit_s'] <= 60, review[0])

if __name__ == '__main__':
    unittest.main()
