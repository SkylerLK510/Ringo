"""Regression coverage for worker review findings; no live inference."""
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from local_worker.adapters import OllamaHttpAdapter, InferenceUnavailable, build_adapter
from local_worker.checks import run_check
from local_worker.config import ConfigError
from local_worker.control import Deadline, StopControl
from local_worker.ledger import OperationLedger
from local_worker.schemas import CheckSpec
from local_worker_support import make_config


class ReviewRegressions(unittest.TestCase):
    def test_live_admission_requires_real_cancellation_proof(self):
        with self.assertRaisesRegex(InferenceUnavailable, "disabled"):
            build_adapter(make_config())

    def test_options_cannot_override_limits(self):
        for key in ("num_ctx", "num_predict", "seed", "temperature"):
            with self.subTest(key=key), self.assertRaises(ConfigError):
                make_config(options={key: -1})

    def test_incomplete_stream_is_refused_and_reads_are_bounded(self):
        response = mock.Mock(status=200)
        response.readline.side_effect = [b'{"response":"{}","done":false}\n', b'']
        conn = mock.Mock()
        conn.getresponse.return_value = response
        config = make_config(allow_unpinned_model=True, model_digest=None)
        with mock.patch('local_worker.adapters.http.client.HTTPConnection', return_value=conn), mock.patch('local_worker.adapters.SocketGuard'):
            with self.assertRaisesRegex(InferenceUnavailable, 'terminal'):
                OllamaHttpAdapter(config).generate('prompt', StopControl(Deadline(5)))
        for call in response.readline.call_args_list:
            self.assertLessEqual(call.args[0], config.max_stream_bytes + 1)
            self.assertGreater(call.args[0], 0)

    def test_reconcile_preserves_active_owner_and_recovers_abandoned_job(self):
        with tempfile.TemporaryDirectory() as temp:
            ledger = OperationLedger(temp)
            with ledger.execution_lock('request-1') as owned:
                self.assertTrue(owned)
                ledger.claim('request-1', 'a', 'b')
                self.assertEqual(ledger.reconcile(), [])
                self.assertEqual(ledger.load('request-1').state, 'running')
            self.assertEqual(len(ledger.reconcile()), 1)
            self.assertEqual(ledger.load('request-1').state, 'unknown')

    def test_expired_check_never_spawns(self):
        spec = CheckSpec.from_dict({'id':'check', 'argv':[sys.executable, '-c', 'pass'], 'timeout_s':1})
        with tempfile.TemporaryDirectory() as temp, mock.patch('local_worker.checks.subprocess.Popen') as spawn:
            result, _ = run_check(spec, Path(temp), Path(temp), Deadline(0), make_config())
            self.assertEqual(result.status, 'timeout')
            spawn.assert_not_called()

    @unittest.skipUnless(os.name == 'posix', 'POSIX groups')
    def test_successful_check_kills_background_descendant(self):
        with tempfile.TemporaryDirectory() as temp:
            pidfile = Path(temp) / 'child.pid'
            script = ('import subprocess,sys,pathlib; p=subprocess.Popen([sys.executable,"-c","import time; time.sleep(30)"]); '
                      f'pathlib.Path({str(pidfile)!r}).write_text(str(p.pid))')
            spec = CheckSpec.from_dict({'id':'check', 'argv':[sys.executable, '-c', script], 'timeout_s':2})
            result, _ = run_check(spec, Path(temp), Path(temp), Deadline(5), make_config())
            self.assertEqual(result.status, 'passed')
            pid = int(pidfile.read_text())
            for _ in range(100):
                stat = Path(f'/proc/{pid}/stat')
                if not stat.exists() or stat.read_text().split()[2] == 'Z':
                    break
                time.sleep(.01)
            else:
                os.kill(pid, 9)
                self.fail('background process survived check cleanup')

    def test_nonowner_result_cannot_overwrite_authoritative_result(self):
        from local_worker.runner import RunRequest, _empty_result, _persist
        from local_worker.adapters import ScriptedAdapter
        from local_worker_support import make_assignment
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            ledger = OperationLedger(root / 'ledger')
            request = RunRequest(make_assignment(), make_config(), ScriptedAdapter([]), lambda: None,
                                 ledger, 'request-1', root / 'results')
            record, _ = ledger.claim('request-1', 'a', 'b')
            result = _empty_result(request=request, operation_id=record.operation_id, status='failed',
                                   phase='test', errors=(), elapsed_ms=0)
            canonical = _persist(request, record, result, finalize=True)
            before = canonical.read_bytes()
            refusal = _persist(request, record, result, finalize=False)
            self.assertNotEqual(canonical, refusal)
            self.assertEqual(canonical.read_bytes(), before)
            self.assertEqual(ledger.load('request-1').result_ref, canonical.name)

    def test_workspace_setup_uses_remaining_job_deadline(self):
        from local_worker.workspace import GitWorktreeWorkspace, WorkspaceError
        from local_worker_support import make_assignment
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            workspace = GitWorktreeWorkspace(make_assignment(), make_config(), root, root / 'private')
            workspace.deadline = Deadline(.1)
            failure = subprocess.CompletedProcess([], 1, '', 'fixture refusal')
            with mock.patch.object(workspace, '_git', return_value=failure) as git:
                with self.assertRaises(WorkspaceError):
                    workspace.__enter__()
                self.assertLessEqual(git.call_args.kwargs['timeout'], .1)
            self.assertFalse(list((root / 'private').iterdir()))
