"""Durable run state and deterministic waiting: what an agent reads instead of re-entering its model to poll."""
import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

import harness
import ringo
import workflow
from tests.test_harness import REVIEW


def state(folder):
    return json.loads((Path(folder) / 'state.json').read_text())


class RunStateTest(unittest.TestCase):
    def setUp(self):
        d = tempfile.TemporaryDirectory()
        self.addCleanup(d.cleanup)
        self.root = Path(d.name)

    def review(self, child, *extra):
        (self.root / 'm.txt').write_text('M')
        err = io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            code = harness.main(['review', '--input', str(self.root / 'm.txt'), '--backend',
                                 f'{sys.executable} -c {json.dumps(child)}',
                                 '--trace', str(self.root / 't'), *extra])
        return code, err.getvalue()

    def test_every_record_updates_a_running_state_with_the_last_step(self):
        run = harness.Run(self.root / 't')
        self.assertEqual(state(self.root / 't')['status'], 'running')
        run.reserve()
        run.record({'step': 'review', 'attempt': 1, 'status': 'started'})
        s = state(self.root / 't')
        self.assertEqual((s['status'], s['last'], s['calls'], s['pid']),
                         ('running', {'step': 'review', 'attempt': 1, 'status': 'started'}, 1, os.getpid()))
        self.assertLess(len(json.dumps(s)), 1000)

    def test_a_finished_review_says_done_and_where_its_result_is(self):
        code, err = self.review(f'import sys; sys.stdin.read(); print({REVIEW!r})', '--out', str(self.root / 'R.md'))
        self.assertEqual(code, 0, err)
        s = state(self.root / 't')
        self.assertEqual((s['status'], s['calls'], s['result']), ('done', 1, str((self.root / 'R.md').resolve())))
        self.assertTrue(Path(s['summary']).is_file())

    def test_a_failed_review_says_failed_with_its_error(self):
        code, _ = self.review('import sys; sys.exit(3)')
        self.assertEqual(code, 2)
        s = state(self.root / 't')
        self.assertEqual(s['status'], 'failed')
        self.assertIn('exited with 3', s['error'])

    def test_a_setup_failure_after_the_state_exists_ends_as_failed(self):
        (self.root / 'm.txt').write_text('M')
        real = Path.write_text
        def full_disk(path, *args, **kw):
            if path.name == 'material.txt':
                raise OSError(28, 'No space left on device')
            return real(path, *args, **kw)
        with mock.patch.object(Path, 'write_text', full_disk), contextlib.redirect_stderr(io.StringIO()):
            code = harness.main(['review', '--input', str(self.root / 'm.txt'), '--backend', 'cat',
                                 '--trace', str(self.root / 't')])
        self.assertEqual(code, 2)
        self.assertEqual(state(self.root / 't')['status'], 'failed')
        self.assertIn('No space left', state(self.root / 't')['error'])

    def test_work_setup_failure_ends_as_failed(self):
        spec = self.root / 'task.json'
        with mock.patch.object(workflow, 'load_spec', return_value={'max_cost': None, 'unpriced_call_usd': None}), \
                mock.patch.object(workflow, 'write_json', side_effect=OSError('disk full')):
            with self.assertRaises(OSError):
                workflow.run(spec, self.root / 'w')
        self.assertEqual(state(self.root / 'w')['status'], 'failed')

    def test_work_prints_a_compact_outcome_and_records_its_state(self):
        report = {'status': 'needs_revision', 'final': None, 'rounds': [{'round': 1, 'big': 'x' * 5000}],
                  'usage': {'calls': 7, 'untokened_calls': 0, 'input': 10, 'cache_read': 0, 'cache_write': 0,
                            'output': 5, 'cost_usd': 0.0}, 'elapsed_s': 3.21}
        out = io.StringIO()
        with mock.patch.object(workflow, 'run', return_value=report), contextlib.redirect_stdout(out):
            code = workflow.main(['--spec', 'task.json', '--out', str(self.root / 'w')])
        printed = json.loads(out.getvalue())
        self.assertEqual(code, 1)
        self.assertEqual((printed['status'], printed['rounds'], printed['usage']['calls']), ('needs_revision', 1, 7))
        self.assertNotIn('big', out.getvalue())
        self.assertLess(len(out.getvalue()), 600)


class WaitTest(unittest.TestCase):
    def setUp(self):
        d = tempfile.TemporaryDirectory()
        self.addCleanup(d.cleanup)
        self.folder = Path(d.name)

    def wait(self, *args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = ringo.wait([str(self.folder), *args])
        return code, json.loads(out.getvalue())

    def test_exit_codes_follow_the_final_status(self):
        for status, code in (('done', 0), ('ready_for_review', 0), ('needs_revision', 1), ('repeated_candidate', 1),
                             ('budget_exhausted', 1),
                             ('failed', 2), ('error', 2), ('interrupted', 2)):
            with self.subTest(status=status):
                harness.write_state(self.folder, {'version': 1, 'status': status, 'pid': os.getpid(), 'calls': 2})
                got, printed = self.wait('--timeout', '0')
                self.assertEqual((got, printed['status'], printed['calls']), (code, status, 2))
                self.assertNotIn('pid', printed)

    def test_a_running_state_whose_process_is_gone_is_interrupted(self):
        child = subprocess.run([sys.executable, '-c', 'import os; print(os.getpid())'], capture_output=True, text=True)
        harness.write_state(self.folder, {'version': 1, 'status': 'running', 'pid': int(child.stdout)})
        self.assertEqual(self.wait('--timeout', '5')[:1] + (self.wait('--timeout', '5')[1]['status'],), (2, 'interrupted'))

    def test_a_live_run_is_reported_as_running_at_the_timeout(self):
        harness.write_state(self.folder, {'version': 1, 'status': 'running', 'pid': os.getpid()})
        started = time.monotonic()
        code, printed = self.wait('--timeout', '0.2')
        self.assertEqual((code, printed['status']), (3, 'running'))
        self.assertLess(time.monotonic() - started, 3)

    def test_wait_returns_as_soon_as_the_run_finishes(self):
        harness.write_state(self.folder, {'version': 1, 'status': 'running', 'pid': os.getpid()})
        finisher = subprocess.Popen([sys.executable, '-c', 'import sys, time; sys.path.insert(0, sys.argv[2]); import harness; '
                                     'time.sleep(1.5); harness.write_state(sys.argv[1], {"version": 1, "status": "done"})',
                                     str(self.folder), str(Path(harness.__file__).parent)])
        self.addCleanup(finisher.wait)
        started = time.monotonic()
        code, printed = self.wait('--timeout', '30')
        self.assertEqual((code, printed['status']), (0, 'done'))
        self.assertLess(time.monotonic() - started, 10)

    def test_a_run_that_finishes_just_before_the_liveness_check_is_not_interrupted(self):
        child = subprocess.run([sys.executable, '-c', 'import os; print(os.getpid())'], capture_output=True, text=True)
        harness.write_state(self.folder, {'version': 1, 'status': 'running', 'pid': int(child.stdout)})
        real_check = ringo.check_pid
        def finish_then_check(pid):  # the run writes done and exits in the gap between read and check
            harness.write_state(self.folder, {'version': 1, 'status': 'done', 'pid': pid})
            return real_check(pid)
        with mock.patch.object(ringo, 'check_pid', side_effect=finish_then_check):
            self.assertEqual(self.wait('--timeout', '5')[0], 0)

    def test_the_pid_check_tells_a_live_process_from_a_gone_one(self):
        ringo.check_pid(os.getpid())
        child = subprocess.Popen([sys.executable, '-c', 'import sys; sys.stdin.read()'], stdin=subprocess.PIPE)
        ringo.check_pid(child.pid)  # alive, and on Windows not terminated by the check
        self.assertIsNone(child.poll())
        child.communicate(b'')
        with self.assertRaises(ProcessLookupError):
            ringo.check_pid(child.pid)

    def test_no_state_yet_is_not_started(self):
        self.assertEqual(self.wait('--timeout', '0'), (3, {'status': 'not_started', 'folder': str(self.folder)}))


if __name__ == '__main__':
    unittest.main()
