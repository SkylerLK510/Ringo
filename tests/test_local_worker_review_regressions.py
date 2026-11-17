"""Regression coverage for worker review findings; no live inference."""
import json
import os
import subprocess
import socket
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

    @unittest.skipUnless(sys.platform == 'linux' and Path('/proc/self/stat').exists(), 'Linux procfs liveness')
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

    def test_check_setup_expiry_never_spawns(self):
        spec = CheckSpec.from_dict({'id':'check', 'argv':[sys.executable, '-c', 'pass'], 'timeout_s':1})
        with tempfile.TemporaryDirectory() as temp:
            deadline = mock.Mock()
            deadline.expired.side_effect = [False, True]
            with mock.patch('local_worker.checks.subprocess.Popen') as spawn:
                result, _ = run_check(spec, Path(temp), Path(temp), deadline, make_config())
            self.assertEqual(result.status, 'timeout')
            spawn.assert_not_called()

    def test_nonposix_checks_refuse_without_spawn(self):
        spec = CheckSpec.from_dict({'id':'check', 'argv':[sys.executable, '-c', 'pass'], 'timeout_s':1})
        with mock.patch('local_worker.checks.os.name', 'nt'), mock.patch('local_worker.checks.subprocess.Popen') as spawn:
            result, _ = run_check(spec, None, None, Deadline(5), make_config())
        self.assertEqual(result.status, 'unavailable')
        spawn.assert_not_called()

    def test_allowed_cleanup_does_not_expire_verified_candidate(self):
        from local_worker.adapters import ScriptedAdapter
        from local_worker.runner import RunRequest, run
        from local_worker.schemas import CheckResult
        from local_worker.workspace import SnapshotWorkspace
        from local_worker_support import make_assignment, candidate_json
        clock = [0.0]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            assignment = make_assignment()
            config = make_config()
            class CleanupWorkspace(SnapshotWorkspace):
                def __exit__(self, *args):
                    clock[0] = config.job_deadline_s + 0.01
                    super().__exit__(*args)
            request = RunRequest(assignment, config, ScriptedAdapter([candidate_json(assignment=assignment)]),
                                 lambda: CleanupWorkspace(assignment, config, root / 'private'),
                                 OperationLedger(root / 'ledger'), 'cleanup-job', root / 'results')
            passed = CheckResult(assignment.checks[0].id, 'passed', 0, 'digest', 0, '')
            with mock.patch('local_worker.control.time.monotonic', side_effect=lambda: clock[0]), mock.patch('local_worker.runner.run_check', return_value=(passed, None)):
                result, _ = run(request)
            self.assertEqual(result.status, 'candidate_ready')

    def test_missing_containment_never_executes_on_host(self):
        spec = CheckSpec.from_dict({'id': 'check', 'argv': [sys.executable, '-c', 'pass'], 'timeout_s': 1})
        with tempfile.TemporaryDirectory() as temp, mock.patch('local_worker.checks.shutil.which', return_value=None), mock.patch('local_worker.checks.subprocess.Popen') as spawn:
            result, _ = run_check(spec, Path(temp), Path(temp), Deadline(5), make_config())
        self.assertEqual(result.status, 'unavailable')
        spawn.assert_not_called()

    @unittest.skipUnless(sys.platform == 'linux', 'Linux namespace containment')
    def test_candidate_check_cannot_read_owner_files_or_connect_to_host(self):
        import shutil
        if not shutil.which('bwrap'):
            self.skipTest('bubblewrap unavailable')
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            owner = root / 'owner-secret'
            owner.write_text('private')
            workspace = root / 'workspace'
            workspace.mkdir()
            host = socket.socket()
            self.addCleanup(host.close)
            host.bind(('127.0.0.1', 0))
            host.listen()
            script = ('from pathlib import Path; import socket; '
                      f'assert not Path({str(owner)!r}).exists(); '
                      f'assert socket.socket().connect_ex(("127.0.0.1", {host.getsockname()[1]})) != 0; '
                      'Path("proof").write_text("contained")')
            spec = CheckSpec.from_dict({'id': 'contained', 'argv': [sys.executable, '-c', script], 'timeout_s': 2})
            result, _ = run_check(spec, workspace, root / 'private', Deadline(5), make_config())
            self.assertEqual(result.status, 'passed', result.detail)
            self.assertEqual((workspace / 'proof').read_text(), 'contained')
            self.assertEqual(owner.read_text(), 'private')

    def test_deep_json_finishes_as_malformed_instead_of_stranding_ledger(self):
        from local_worker.adapters import ScriptedAdapter
        from local_worker.runner import RunRequest, run
        from local_worker_support import make_assignment, workspace_factory
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            assignment, config = make_assignment(), make_config()
            ledger = OperationLedger(root / 'ledger')
            adapter = ScriptedAdapter(['[' * 2000 + '0' + ']' * 2000])
            request = RunRequest(assignment, config, adapter, workspace_factory(assignment, config, root / 'private'), ledger, 'deep-json', root / 'results')
            result, _ = run(request)
            self.assertEqual(result.status, 'malformed_candidate')
            self.assertNotEqual(ledger.load('deep-json').state, 'running')
            self.assertEqual(len(adapter.prompts), 1)

    def test_invalid_workspace_is_rejected_before_inference_and_finalized(self):
        from local_worker.adapters import ScriptedAdapter
        from local_worker.runner import RunRequest, run
        from local_worker.workspace import WorkspaceError
        from local_worker_support import make_assignment
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            ledger = OperationLedger(root / 'ledger')
            adapter = ScriptedAdapter([])
            factory = mock.Mock(side_effect=WorkspaceError('missing base revision'))
            request = RunRequest(make_assignment(), make_config(), adapter, factory, ledger, 'bad-workspace', root / 'results')
            result, _ = run(request)
            self.assertNotEqual(result.status, 'candidate_ready')
            self.assertNotEqual(ledger.load('bad-workspace').state, 'running')
            self.assertEqual(adapter.prompts, [])
