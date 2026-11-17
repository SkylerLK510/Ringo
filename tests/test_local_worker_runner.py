import json
import os
import socket
import tempfile
import threading
import unittest
from pathlib import Path

from local_worker.adapters import InferenceCancelled, ScriptedAdapter, UnsupportedPolicy, build_adapter
from local_worker.control import Deadline, SocketGuard, StopControl
from local_worker.ledger import OperationConflict, OperationLedger
from local_worker.runner import RunRequest, run

from local_worker_support import (
    SANDBOX_AVAILABLE,
    candidate_json,
    failing_check,
    make_assignment,
    make_config,
    passing_check,
    workspace_factory,
)


def _hang(prompt, stop):
    signal = threading.Event()
    while not stop.stopped:
        signal.wait(min(0.05, stop.poll_interval()))
    raise InferenceCancelled("hung request stopped")


@unittest.skipUnless(SANDBOX_AVAILABLE, "working Linux bubblewrap and visible Python required")
class RunnerTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        self.private_root = root / "private"
        self.result_dir = root / "results"
        self.ledger = OperationLedger(root / "ledger")

    def _run(self, assignment, config, script, *, request_id="job-1"):
        adapter = ScriptedAdapter(script)
        request = RunRequest(
            assignment=assignment,
            config=config,
            adapter=adapter,
            workspace_factory=workspace_factory(assignment, config, self.private_root),
            ledger=self.ledger,
            request_id=request_id,
            result_dir=self.result_dir,
        )
        return run(request), adapter

    @unittest.skipUnless(os.name == "posix", "real checks require POSIX process cleanup")
    def test_all_configured_checks_run_and_must_pass(self):
        # A passing first check must not make a later configured check optional.
        assignment = make_assignment(checks=[passing_check("first"), passing_check("second")])
        config = make_config()
        (result, result_path), adapter = self._run(
            assignment, config, [candidate_json(assignment=assignment)]
        )
        self.assertEqual(result.status, "candidate_ready")
        self.assertEqual(len(adapter.prompts), 1)
        self.assertEqual([item.check_id for item in result.check_results], ["first", "second"])
        self.assertEqual(result.attempts[0].status, "completed")
        self.assertIsNotNone(result.candidate_ref)
        self.assertTrue((self.result_dir / result.candidate_ref).exists())
        self.assertTrue(result_path.exists())

    @unittest.skipUnless(os.name == "posix", "real checks require POSIX process cleanup")
    def test_repair_failing_early_check_is_not_accepted(self):
        assignment = make_assignment(checks=[failing_check("first"), passing_check("second")])
        config = make_config()
        script = [candidate_json(assignment=assignment)] * 2
        (result, _), adapter = self._run(assignment, config, script)
        self.assertEqual(result.status, "check_failed")
        self.assertEqual(len(adapter.prompts), 2)
        self.assertEqual(
            [item.check_id for item in result.check_results],
            ["first", "second", "first", "second"],
        )
        self.assertEqual(adapter.remaining, 0)

    @unittest.skipUnless(os.name == "posix", "real checks require POSIX process cleanup")
    def test_at_most_two_suites_and_no_third_call(self):
        assignment = make_assignment(checks=[failing_check("first"), failing_check("second")])
        config = make_config()
        script = [candidate_json(assignment=assignment)] * 3
        (result, _), adapter = self._run(assignment, config, script)
        self.assertEqual(result.status, "check_failed")
        self.assertEqual(len(adapter.prompts), 2)
        self.assertEqual(len(result.check_results), 4)
        self.assertEqual(adapter.remaining, 1)

    def test_deadline_consumes_attempt_and_stops_further_calls(self):
        assignment = make_assignment(checks=[])
        config = make_config(job_deadline_s=1.0, cleanup_allowance_s=0.1, attempt_deadline_s=1.0)
        (result, _), adapter = self._run(assignment, config, [_hang, candidate_json(assignment=assignment)])
        self.assertEqual(result.status, "timeout")
        self.assertEqual(len(adapter.prompts), 1)
        self.assertEqual(adapter.cancelled, 1)

    def test_invalid_candidate_shape_writes_a_terminal_result(self):
        assignment = make_assignment(checks=[passing_check()])
        config = make_config()
        (result, result_path), adapter = self._run(assignment, config, ["[]"])
        self.assertEqual(result.status, "malformed_candidate")
        self.assertTrue(result.errors[0].startswith("invalid_shape"))
        self.assertTrue(result_path.exists())
        self.assertEqual(len(adapter.prompts), 1)

    def test_existing_running_operation_is_not_executed_again(self):
        assignment = make_assignment(checks=[passing_check()])
        config = make_config()
        # Claim without finalizing: simulates an interrupted run.
        self.ledger.begin("job-1", assignment.digest(), config.fingerprint())
        (result, result_path), adapter = self._run(
            assignment, config, [candidate_json(assignment=assignment)]
        )
        self.assertEqual(result.status, "unavailable")
        self.assertEqual(result.phase, "reconcile")
        self.assertEqual(adapter.prompts, [])
        self.assertEqual(adapter.remaining, 1)
        self.assertTrue(result_path.exists())
        # The existing running record was not overwritten by this refusal.
        self.assertEqual(self.ledger.find(result.operation_id).state, "running")

    def test_concurrent_claims_are_exclusive(self):
        digest = "sha256:" + "1" * 64
        fingerprint = "sha256:" + "2" * 64
        outcomes = []
        barrier = threading.Barrier(4)

        def worker():
            barrier.wait()
            outcomes.append(self.ledger.claim("race", digest, fingerprint))

        threads = [threading.Thread(target=worker) for _ in range(4)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=5.0)
        self.assertEqual(len(outcomes), 4)
        self.assertEqual(sum(1 for _, created in outcomes if created), 1)
        self.assertEqual(len({record.operation_id for record, _ in outcomes}), 1)

    def test_cloud_policy_is_refused_without_a_call(self):
        with self.assertRaises(UnsupportedPolicy):
            build_adapter(make_config(policy="cloud"))
        with self.assertRaises(UnsupportedPolicy):
            build_adapter(make_config(policy="auto"))

    def test_identical_retry_returns_existing_operation_and_conflict_fails(self):
        first = self.ledger.begin("req-1", "sha256:" + "1" * 64, "sha256:" + "2" * 64)
        again = self.ledger.begin("req-1", "sha256:" + "1" * 64, "sha256:" + "2" * 64)
        self.assertEqual(first.operation_id, again.operation_id)
        with self.assertRaises(OperationConflict):
            self.ledger.begin("req-1", "sha256:" + "1" * 64, "sha256:" + "3" * 64)
        running = self.ledger.find(first.operation_id)
        self.assertEqual(running.state, "running")
        changed = self.ledger.reconcile()
        self.assertEqual(changed[0].state, "unknown")


class CancellationTests(unittest.TestCase):
    def test_socket_guard_stops_only_the_owned_request(self):
        owned, peer = socket.socketpair()
        unrelated = socket.socket()
        self.addCleanup(unrelated.close)
        try:
            guard = SocketGuard(owned, StopControl(Deadline(0.05)))
            guard.start()
            peer.settimeout(2.0)
            self.assertEqual(peer.recv(16), b"")
            guard.finish()
            self.assertTrue(guard.triggered)
            unrelated.bind(("127.0.0.1", 0))
            self.assertIsNotNone(unrelated.getsockname())
        finally:
            owned.close()
            peer.close()
