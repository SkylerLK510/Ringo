"""Offline optimization cases: integrity before inference and honest timing denominators.

These are correctness tests, not performance observations or an RRSI campaign.
Subprocess scheduling is mocked only in the measurement-runner cases; release
cases launch real fresh Python processes against a local fixture backend.
"""
import contextlib
import copy
import io
import itertools
import json
import os
from pathlib import Path
import subprocess
import unittest
from unittest import mock

import bench_latency
import releases
from tests import test_releases as fixtures


class OptimizationCases(unittest.TestCase):
    def fixture(self):
        case = fixtures.ReleaseTest()
        self.addCleanup(case.doCleanups)
        case.setUp()
        return case

    def damage(self, case, kind):
        if kind in ('source', 'bytecode'):
            path = case.bundle / 'harness.py' if kind == 'source' else next(
                (case.bundle / '__pycache__').glob('harness.*.pyc'))
            path.chmod(0o644)
            path.write_bytes(path.read_bytes() + b'corrupt')
        elif kind == 'missing-prompt':
            path = case.bundle / 'prompts/work.md'
            path.parent.chmod(0o755)
            path.unlink()
        elif kind == 'unexpected-file':
            case.bundle.chmod(0o755)
            path = case.bundle / 'extra' / 'nested' / 'café.txt'
            path.parent.mkdir(parents=True)
            path.write_text('unexpected', encoding='utf-8')
        elif kind == 'dangling-symlink':
            case.bundle.chmod(0o755)
            (case.bundle / 'dangling').symlink_to(case.root / 'absent')
        elif kind == 'manifest-symlink':
            case.bundle.chmod(0o755)
            path = case.bundle / 'release.json'
            target = case.root / 'manifest-copy.json'
            target.write_bytes(path.read_bytes())
            path.unlink()
            path.symlink_to(target)
        elif kind == 'fifo':
            case.bundle.chmod(0o755)
            os.mkfifo(case.bundle / 'unregistered-pipe')
        elif kind == 'approval':
            entry = releases.pointer(case.state)['active']
            path = case.state / 'approvals' / (entry['approval'] + '.json')
            record = json.loads(path.read_text())
            record['approved_by'] = 'altered after approval'
            path.write_text(json.dumps(record))
        else:
            raise AssertionError(kind)

    def test_corrupt_release_cases_fail_before_inference_or_trace_creation(self):
        kinds = ['source', 'bytecode', 'missing-prompt', 'unexpected-file',
                 'dangling-symlink', 'manifest-symlink', 'approval']
        if hasattr(os, 'mkfifo'):
            kinds.append('fifo')
        for kind in kinds:
            with self.subTest(kind=kind):
                case = self.fixture()
                state = copy.deepcopy(case.activate())
                self.damage(case, kind)
                trace = case.root / 'rejected-trace'
                result = case.cli(case.review_args(trace))
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn('ringo release:', result.stderr)
                self.assertFalse((case.root / 'calls').exists())
                self.assertFalse(trace.exists())
                self.assertEqual(releases.pointer(case.state), state)

    def test_fresh_invocation_rechecks_after_a_success_instead_of_reusing_it(self):
        for kind in ('source', 'bytecode', 'approval'):
            with self.subTest(kind=kind):
                case = self.fixture()
                case.activate()
                first = case.cli(case.review_args(case.root / 'first-trace'))
                self.assertEqual(first.returncode, 0, first.stderr)
                self.assertEqual((case.root / 'calls').read_text().splitlines(), ['call'])
                self.damage(case, kind)
                second = case.cli(case.review_args(case.root / 'second-trace'))
                self.assertEqual(second.returncode, 2, second.stderr)
                self.assertEqual((case.root / 'calls').read_text().splitlines(), ['call'])
                self.assertFalse((case.root / 'second-trace').exists())

    def test_normal_optimized_review_keeps_one_call_and_exact_provenance(self):
        case = self.fixture()
        state = case.activate()
        trace = case.root / 'trace'
        result = case.cli(case.review_args(trace))
        self.assertEqual(result.returncode, 0, result.stderr)
        summary = json.loads((trace / 'summary.json').read_text())
        identity = summary['implementation']
        self.assertEqual(summary['calls'], 1)
        self.assertEqual((case.root / 'calls').read_text().splitlines(), ['call'])
        self.assertEqual(identity['mode'], 'release')
        self.assertEqual(identity['id'], case.rid)
        self.assertEqual(identity['approval'], state['active']['approval'])
        self.assertEqual(identity['bootstrap_sha256'], releases.sha({
            n: releases.file_hash(case.source / n)
            for n in ('ringo.py', 'releases.py', 'latency.py')}))
        self.assertEqual(identity['plan_sha256'], case.manifest['default_review_sha256'])

    def test_measurement_schedule_keeps_all_workloads_warmups_failures_and_timeouts(self):
        case = self.fixture()
        out = case.root / 'schedule'
        commands = []

        def execute(command, **kwargs):
            commands.append((command, kwargs['env']))
            if any('review-trace-0-candidate-trace' in str(arg) for arg in command):
                raise subprocess.TimeoutExpired(command, 45)
            code = 2 if kwargs['env']['RINGO_FAKE_FAIL'] == '1' else 0
            return subprocess.CompletedProcess(command, code, b'', b'fixture stderr')

        before = releases.pointer(releases.state_root())
        clock = itertools.count(1_000_000, 1_000_000)
        with mock.patch.object(bench_latency.subprocess, 'run', side_effect=execute), \
                mock.patch.object(bench_latency.time, 'perf_counter_ns', side_effect=lambda: next(clock)), \
                mock.patch.object(bench_latency.platform, 'platform', return_value='fixture-host'), \
                contextlib.redirect_stdout(io.StringIO()):
            suite = bench_latency.run(case.source, out, pairs=30, warmups=2)
        self.assertEqual(releases.pointer(releases.state_root()), before)
        self.assertEqual(len(commands), 9 * (30 + 2) * 2)
        events = [json.loads(line) for line in (out / 'execution.jsonl').read_text().splitlines()]
        self.assertEqual(len(events), len(commands))
        reports = {r['measurement']['protocol']['workload']: r for r in suite['reports']}
        self.assertEqual(set(reports), releases.LATENCY_WORKLOADS | {
            'review-delay', 'review-override', 'review-failure'})
        for report in reports.values():
            measurement = report['measurement']
            self.assertEqual(len(measurement['pairs']), 30)
            self.assertEqual([p['id'] for p in measurement['pairs']], [str(i) for i in range(30)])
            self.assertEqual(measurement['protocol']['warmups'], 2)
            self.assertEqual(measurement['protocol']['kind'], 'simulated')
            self.assertEqual(report['status'], 'slower' if report is reports['review-trace'] else 'uncertain')
        self.assertEqual(reports['review-failure']['failures'], {'baseline': 30, 'candidate': 30})
        self.assertEqual(reports['review-trace']['failures'], {'baseline': 0, 'candidate': 1})
        timeouts = [e for e in events if e['exit'] == 'timeout']
        self.assertEqual(len(timeouts), 1)
        self.assertFalse(timeouts[0]['ok'])
        self.assertEqual(len([e for e in events if e['label'].startswith('review-failure-') and not e['ok']]), 64)
        self.assertTrue(all(env['RINGO_RELEASE_HOME'] == str(out / 'no-active') for _, env in commands))
        self.assertFalse((out / 'state/active.json').exists())
        self.assertEqual(json.loads((out / 'suite.json').read_text()), suite)


if __name__ == '__main__':
    unittest.main()
