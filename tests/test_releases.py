"""Release integrity, approval, rollback, isolation and in-flight pinning; no model calls."""
import copy
import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

import latency
import pipelines
import releases as rel
from tests.test_latency import data, POLICY


class ReleaseTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.source = self.root / 'source'
        self.source.mkdir()
        for name in rel.RUNTIME:
            shutil.copy2(rel.HERE / name, self.source / name)
        for folder in ('prompts', 'plans'):
            shutil.copytree(rel.HERE / folder, self.source / folder)
        self.state = self.root / 'state'
        self.rid = rel.stage(self.source, self.state)
        self.bundle, self.manifest = rel.verify(self.state, self.rid)
        self.backend = self.root / 'fake.py'
        self.backend.write_text('''import sys, pathlib, time
payload = sys.stdin.read()
root = pathlib.Path(__file__).parent
with (root/'calls').open('a') as f: f.write('call\\n')
if (root/'wait').exists():
    (root/'ready').write_text('ready')
    for _ in range(500):
        if (root/'go').exists(): break
        time.sleep(.01)
print('No defects found.')
''')
        self.env = {**os.environ, 'RINGO_RELEASE_HOME': str(self.state), 'PYTHONDONTWRITEBYTECODE': '1'}

    def report(self, rid=None, baseline='checkout:fixture', slow=False, count=30):
        rid = rid or self.rid
        _, manifest = rel.verify(self.state, rid)
        reports = []
        for workload in sorted(rel.LATENCY_WORKLOADS):
            d = data(1.02 if slow else 1, count=count)
            d.update(baseline=baseline, candidate=rid)
            d['protocol'].update(workload=workload, candidate_config=manifest['default_review_sha256'])
            if isinstance(baseline, str) and rel.IDENT.fullmatch(baseline):
                _, base_manifest = rel.verify(self.state, baseline)
                d['protocol']['baseline_config'] = base_manifest['default_review_sha256']
            reports.append(latency.assess(d, {**POLICY, 'min_pairs': 30}))
        return {'version': 1, 'reports': reports}

    def activate(self, rid=None, expected='none', **kw):
        rid = rid or self.rid
        return rel.activate(self.state, rid, self.report(rid, expected), 'unit-test-human', expected, **kw)

    def next_release(self):
        p = self.source / 'prompts/review.md'
        p.write_text(p.read_text() + '\nUpdated test release.\n')
        return rel.stage(self.source, self.state)

    def cli(self, args, **kw):
        return subprocess.run([sys.executable, str(self.source / 'ringo.py'), *args], env=self.env,
                              text=True, capture_output=True, timeout=30, **kw)

    def review_args(self, trace):
        material = self.root / 'material.txt'
        material.write_text('Review a small fixture.')
        import shlex
        return ['review', '--input', str(material), '--backend', shlex.join([sys.executable, str(self.backend)]),
                '--trace', str(trace), '--out', str(trace.parent / (trace.name + '.md'))]

    def test_stage_is_reproducible_readonly_private_and_does_not_activate(self):
        self.assertEqual(rel.stage(self.source, self.state), self.rid)
        self.assertFalse((self.state / 'active.json').exists())
        self.assertEqual(self.state.stat().st_mode & 0o777, 0o700)
        self.assertEqual((self.bundle / 'ringo.py').stat().st_mode & 0o222, 0)

    def test_source_prompt_changes_change_identity_and_actual_default_plan_hash(self):
        next_id = self.next_release()
        _, second = rel.verify(self.state, next_id)
        self.assertNotEqual(next_id, self.rid)
        self.assertNotEqual(second['default_review_sha256'], self.manifest['default_review_sha256'])

    def test_missing_runtime_file_is_refused(self):
        (self.source / 'context.py').unlink()
        with self.assertRaises(ValueError):
            rel.stage(self.source, self.state)

    def test_symlink_source_and_bundle_are_refused(self):
        p = self.source / 'context.py'
        p.unlink()
        p.symlink_to(rel.HERE / 'context.py')
        with self.assertRaises(ValueError):
            rel.stage(self.source, self.state)
        q = self.bundle / 'context.py'
        self.bundle.chmod(0o755)
        q.unlink()
        q.symlink_to(rel.HERE / 'context.py')
        with self.assertRaises(ValueError):
            rel.verify(self.state, self.rid)

    def test_tampered_content_manifest_and_extra_file_are_refused(self):
        p = self.bundle / 'ringo.py'
        p.chmod(0o644)
        original = p.read_bytes()
        p.write_bytes(original + b'# tampered\n')
        with self.assertRaises(ValueError):
            rel.verify(self.state, self.rid)
        p.write_bytes(original)
        self.bundle.chmod(0o755)
        (self.bundle / 'unexpected.py').write_text('')
        with self.assertRaises(ValueError):
            rel.verify(self.state, self.rid)

    def rewrite_manifest(self, manifest):
        """Construct a self-consistent test release id for manifest validation cases."""
        self.bundle.chmod(0o755)
        path = self.bundle / 'release.json'
        path.chmod(0o644)
        path.write_bytes(rel.canonical(manifest) + b'\n')
        rid = rel.sha(manifest)
        target = self.state / 'bundles' / rid
        if target != self.bundle:
            self.bundle.rename(target)
        self.bundle, self.rid, self.manifest = target, rid, manifest

    def test_verification_accepts_registered_nested_unicode_and_whitespace_names(self):
        self.bundle.chmod(0o755)
        name = 'fixtures/nested folder/café\tvalue.txt'
        path = self.bundle / name
        path.parent.mkdir(parents=True)
        path.write_bytes(b'complete fixture content\n')
        manifest = copy.deepcopy(self.manifest)
        manifest['files'][name] = rel.file_hash(path)
        self.rewrite_manifest(manifest)
        bundle, observed = rel.verify(self.state, self.rid)
        self.assertEqual(bundle, self.bundle)
        self.assertEqual(observed, manifest)

    def test_missing_non_runtime_inventory_file_is_rejected(self):
        parent = self.bundle / 'prompts'
        parent.chmod(0o755)
        (parent / 'work.md').unlink()
        with self.assertRaises(ValueError):
            rel.verify(self.state, self.rid)

    def test_unexpected_deep_file_is_rejected(self):
        self.bundle.chmod(0o755)
        path = self.bundle / 'extra' / 'nested' / 'unexpected.txt'
        path.parent.mkdir(parents=True)
        path.write_text('unregistered')
        with self.assertRaises(ValueError):
            rel.verify(self.state, self.rid)

    def test_empty_unregistered_directory_remains_compatible(self):
        self.bundle.chmod(0o755)
        (self.bundle / 'empty' / 'nested').mkdir(parents=True)
        self.assertEqual(rel.verify(self.state, self.rid)[1], self.manifest)

    def test_directory_and_dangling_symlinks_are_rejected(self):
        self.bundle.chmod(0o755)
        for destination in (self.root, self.root / 'does-not-exist'):
            path = self.bundle / 'linked-directory'
            path.symlink_to(destination, target_is_directory=True)
            with self.subTest(destination=str(destination)), self.assertRaises(ValueError):
                rel.verify(self.state, self.rid)
            path.unlink()

    @unittest.skipUnless(hasattr(os, 'mkfifo'), 'POSIX special-file fixture')
    def test_unregistered_special_file_is_rejected_without_opening_it(self):
        self.bundle.chmod(0o755)
        os.mkfifo(self.bundle / 'unexpected.fifo')
        with self.assertRaises(ValueError):
            rel.verify(self.state, self.rid)

    def test_verification_does_not_reuse_a_previous_success_for_changed_bytecode(self):
        rel.verify(self.state, self.rid)
        path = next((self.bundle / '__pycache__').glob('harness.*.pyc'))
        path.chmod(0o644)
        original = path.read_bytes()
        path.write_bytes(original[:-1] + bytes([original[-1] ^ 1]))
        with self.assertRaises(ValueError):
            rel.verify(self.state, self.rid)

    def test_canonical_manifest_paths_and_hashes_are_required_before_inventory_reads(self):
        original = copy.deepcopy(self.manifest)
        cases = [('../outside', 'a'*64), ('/absolute', 'a'*64), ('nested/../value', 'a'*64),
                 ('./value', 'a'*64), ('nested//value', 'a'*64), ('release.json', 'a'*64),
                 ('', 'a'*64), ('.', 'a'*64), ('extra.txt', None), ('extra.txt', 'A'*64),
                 ('extra.txt', 'short')]
        for name, digest in cases:
            manifest = copy.deepcopy(original)
            manifest['files'][name] = digest
            self.rewrite_manifest(manifest)
            with self.subTest(name=name, digest=digest), self.assertRaises(ValueError):
                rel.verify(self.state, self.rid)

    def test_directory_enumeration_and_content_read_errors_fail_closed(self):
        with mock.patch.object(rel.os, 'scandir', side_effect=PermissionError('fixture directory denied')):
            with self.assertRaises(OSError):
                rel.verify(self.state, self.rid)
        original_hash = rel.file_hash
        def read_error(path):
            if Path(path).name == 'harness.py':
                raise OSError('fixture content read failed')
            return original_hash(path)
        with mock.patch.object(rel, 'file_hash', side_effect=read_error):
            with self.assertRaises(OSError):
                rel.verify(self.state, self.rid)

    def test_invalid_id_and_pointer_json_fail_closed(self):
        for value in ('../escape', '', 'g'*64):
            with self.subTest(value=value), self.assertRaises(ValueError):
                rel.verify(self.state, value)
        (self.state / 'active.json').write_text('{"version":1,"active":null,"active":null}')
        with self.assertRaises(ValueError):
            rel.pointer(self.state)

    def test_activation_and_rollback_preserve_previous_approval(self):
        a = self.activate()
        bid = self.next_release()
        b = self.activate(bid, self.rid)
        self.assertEqual(b['previous'], a['active'])
        rolled = rel.rollback(self.state, bid)
        self.assertEqual(rolled['active'], a['active'])
        self.assertEqual(rolled['previous'], b['active'])

    def test_unexpected_current_cannot_overwrite_pointer(self):
        state = self.activate()
        with self.assertRaises(ValueError):
            self.activate(self.next_release(), 'none')
        self.assertEqual(rel.pointer(self.state), state)

    def test_rollback_without_previous_or_with_corrupt_previous_keeps_current(self):
        state = self.activate()
        with self.assertRaises(ValueError):
            rel.rollback(self.state, self.rid)
        bid = self.next_release()
        state = self.activate(bid, self.rid)
        p = self.bundle / 'context.py'
        p.chmod(0o644)
        p.write_text('changed')
        with self.assertRaises(ValueError):
            rel.rollback(self.state, bid)
        self.assertEqual(rel.pointer(self.state), state)

    def test_slow_and_uncertain_reports_do_not_auto_activate(self):
        with self.assertRaisesRegex(ValueError, 'slowdown'):
            rel.activate(self.state, self.rid, self.report(slow=True), 'human', 'none', 'even an override')
        short = self.report(count=10)
        with self.assertRaisesRegex(ValueError, 'uncertain'):
            rel.activate(self.state, self.rid, short, 'human', 'none')
        self.assertIsNone(rel.pointer(self.state)['active'])
        rel.activate(self.state, self.rid, short, 'human', 'none', 'Explicit unit-test decision')

    def test_full_workload_coverage_and_matching_config_are_required(self):
        report = self.report()
        report['reports'].pop()
        with self.assertRaisesRegex(ValueError, 'coverage'):
            rel.activate(self.state, self.rid, report, 'human', 'none')
        report = self.report()
        item = report['reports'][0]
        item['measurement']['protocol']['candidate_config'] = 'wrong'
        report['reports'][0] = latency.assess(item['measurement'], item['policy'])
        with self.assertRaisesRegex(ValueError, 'configuration'):
            rel.activate(self.state, self.rid, report, 'human', 'none')

    def test_forged_verdict_and_wrong_candidate_or_baseline_are_rejected(self):
        for change in ('status', 'candidate'):
            report = self.report()
            report['reports'][0][change] = 'forged' if change == 'status' else 'x'
            with self.subTest(change=change), self.assertRaises(ValueError):
                rel.activate(self.state, self.rid, report, 'human', 'none')
        self.activate()
        bid = self.next_release()
        with self.assertRaisesRegex(ValueError, 'baseline'):
            rel.activate(self.state, bid, self.report(bid, 'wrong'), 'human', self.rid)

    def test_approval_tamper_prevents_launch(self):
        state = self.activate()
        path = self.state / 'approvals' / (state['active']['approval'] + '.json')
        path.write_text('{}')
        result = self.cli(['--help'])
        self.assertEqual(result.returncode, 2)
        self.assertIn('approval integrity', result.stderr)

    def test_failed_atomic_pointer_replace_preserves_current_state(self):
        original = self.activate()
        next_id = self.next_release()
        real_replace = rel.os.replace
        def fail(source, target):
            if target == self.state / 'active.json':
                raise OSError('simulated pointer write failure')
            return real_replace(source, target)
        with mock.patch.object(rel.os, 'replace', side_effect=fail), self.assertRaises(OSError):
            self.activate(next_id, self.rid)
        self.assertEqual(rel.pointer(self.state), original)

    def test_candidate_without_sealed_test_evidence_is_refused(self):
        with self.assertRaises(ValueError):
            rel.stage(self.source, self.state, self.source / 'plans/review.json')

    def test_staging_rejects_external_default_prompt_directory(self):
        path = self.source / 'plans/review.json'
        raw = json.loads(path.read_text())
        raw['prompts_dir'] = str(rel.HERE / 'prompts')
        path.write_text(json.dumps(raw))
        with self.assertRaisesRegex(ValueError, 'inside their bundle'):
            rel.stage(self.source, self.state)

    def test_concurrent_activations_only_one_can_replace_expected_current(self):
        self.activate()
        bid = self.next_release()
        report = self.report(bid, self.rid)
        barrier, outcomes = threading.Barrier(2), []
        def worker():
            barrier.wait()
            try:
                rel.activate(self.state, bid, report, 'test-human', self.rid)
                outcomes.append('activated')
            except ValueError:
                outcomes.append('stale')
        threads = [threading.Thread(target=worker) for _ in range(2)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(10)
            self.assertFalse(t.is_alive())
        self.assertCountEqual(outcomes, ['activated', 'stale'])

    def test_preview_work_uses_exactly_one_writer_and_six_judges(self):
        from tests.test_workflow import FAKE_BRIDGE
        bridge = self.root / 'bridge.py'
        bridge.write_text(FAKE_BRIDGE)
        self.backend.write_text('''import sys,json
text=sys.stdin.read()
with open(__file__+'.calls','a') as f: f.write('call\\n')
if text.startswith('Implement the task'):
    print(json.dumps({'files':{'value.py':'def f(x): return 2*x\\n'},'rationale':'fixture'}))
else:
    print(json.dumps({'accept':True}))
''')
        import shlex
        backend = shlex.join([sys.executable, str(self.backend)])
        task = self.root / 'work.json'
        task.write_text(json.dumps({'version': 1, 'brief': 'Twice.\n\nHard constraints:\n- f(5) is 10.',
            'files': {'value.py': ''}, 'checks': [{'name':'fixture','argv':[sys.executable,'-c','pass']}],
            'allow_execution': True, 'allow_tool_backends': True, 'writer':backend,'judge':backend,
            'council':[sys.executable,str(bridge)],'rubric':str(self.root/'rubric.json'),
            'max_rounds':1,'samples':6,'max_seconds':30}))
        result = self.cli(['release','preview','--release',self.rid,'--','work','--spec',str(task),
                           '--out',str(self.root/'work-run')])
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        report = json.loads((self.root/'work-run/report.json').read_text())
        self.assertEqual(report['usage']['calls'], 7)
        self.assertEqual(report['implementation']['id'], self.rid)
        self.assertEqual(len(Path(str(self.backend)+'.calls').read_text().splitlines()), 7)

    def test_quality_requires_test_pass_and_matching_candidate(self):
        import evaluate
        plan = rel.default_plan(self.source)
        spec = {'split': 'test', 'candidate': 'cand', 'comparators': ['base'], 'objective': 'quality', 'arms': {'cand': {
            'plan': str(self.source / 'plans/review.json'), 'backend': 'claude'}}}
        decision = {'kind': 'decision', 'digest': 'd', 'result': 'pass'}
        with mock.patch.object(evaluate, 'checked', return_value=spec), \
             mock.patch.object(evaluate, 'ledger', return_value=[decision]):
            evidence = rel.quality(self.root / 'store', 'd', plan)
            self.assertEqual(evidence['kind'], 'rrsi')
            for split, result in [('dev', 'pass'), ('validation', 'pass'), ('test', 'fail')]:
                spec['split'], decision['result'] = split, result
                with self.subTest(split=split), self.assertRaises(ValueError):
                    rel.quality(self.root / 'store', 'd', plan)

    def test_sealed_test_claim_cannot_transfer_to_different_staged_review_code(self):
        import evaluate
        spec = {'split': 'test', 'candidate': 'cand', 'comparators': ['base'], 'objective': 'quality', 'arms': {'cand': {
            'plan': str(self.source / 'plans/review.json'), 'backend': 'claude'}}}
        decision = {'kind': 'decision', 'digest': 'd', 'result': 'pass'}
        with mock.patch.object(evaluate, 'checked', return_value=spec), \
                mock.patch.object(evaluate, 'ledger', return_value=[decision]):
            evidence = rel.quality(self.root / 'store', 'd', rel.default_plan(self.source), self.source)
            self.assertEqual(evidence['implementation_sha256'], rel.sha({
                n: rel.file_hash(evaluate.HERE / n) for n in evaluate.CODE}))
            for name in evaluate.CODE:
                path = self.source / name
                original = path.read_bytes()
                path.write_bytes(original + b'\n# unevaluated implementation\n')
                try:
                    with self.subTest(name=name), self.assertRaisesRegex(ValueError, 'implementation differs'):
                        rel.stage(self.source, self.state, self.source / 'plans/review.json',
                                  self.root / 'store', 'd')
                finally:
                    path.write_bytes(original)
            self.assertFalse((self.state / 'active.json').exists())
            self.assertEqual(len(list((self.state / 'bundles').iterdir())), 1)

    def test_activation_rechecks_the_review_implementation_bound_to_quality(self):
        import evaluate
        spec = {'split': 'test', 'candidate': 'cand', 'comparators': ['base'], 'objective': 'quality', 'arms': {'cand': {
            'plan': str(self.source / 'plans/review.json'), 'backend': 'claude'}}}
        decision = {'kind': 'decision', 'digest': 'd', 'result': 'pass'}
        with mock.patch.object(evaluate, 'checked', return_value=spec), \
                mock.patch.object(evaluate, 'ledger', return_value=[decision]):
            evidence = rel.quality(self.root / 'store', 'd', rel.default_plan(self.source), self.source)
            # Self-consistent forged bundle: its inventory hashes are valid but
            # its review code differs from the implementation covered by proof.
            path = self.bundle / 'harness.py'
            path.chmod(0o644)
            path.write_bytes(path.read_bytes() + b'\n# unevaluated implementation\n')
            manifest = copy.deepcopy(self.manifest)
            manifest['files']['harness.py'] = rel.file_hash(path)
            manifest['quality'] = evidence
            self.rewrite_manifest(manifest)
            rel.verify(self.state, self.rid)
            with self.assertRaisesRegex(ValueError, 'implementation differs'):
                self.activate()
            self.assertFalse((self.state / 'active.json').exists())

    def test_matching_comparative_proof_can_stage_and_activate_in_fixture_store(self):
        import evaluate
        spec = {'split': 'test', 'candidate': 'cand', 'comparators': ['base'], 'objective': 'quality',
                'arms': {'cand': {'plan': str(self.source / 'plans/review.json'), 'backend': 'claude'}}}
        decision = {'kind': 'decision', 'digest': 'fixture', 'result': 'pass'}
        with mock.patch.object(evaluate, 'checked', return_value=spec), \
                mock.patch.object(evaluate, 'ledger', return_value=[decision]):
            rid = rel.stage(self.source, self.state, self.source / 'plans/review.json',
                            self.root / 'fixture-store', 'fixture')
            _, manifest = rel.verify(self.state, rid)
            evidence = manifest['quality']
            self.assertEqual(evidence['comparators'], ['base'])
            self.assertEqual(evidence['objective'], 'quality')
            self.assertEqual(evidence['implementation_sha256'], rel.sha({
                name: rel.file_hash(evaluate.HERE / name) for name in evaluate.CODE}))
            state = self.activate(rid)
            self.assertEqual(rel.approved(self.state, state['active'])['quality'], evidence)

    def test_preview_runs_bundled_modules_without_approval_and_one_backend_call(self):
        trace = self.root / 'trace'
        proc = self.cli(['release', 'preview', '--release', self.rid, '--', *self.review_args(trace)])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        report = json.loads((trace / 'summary.json').read_text())
        self.assertEqual(report['calls'], 1)
        self.assertEqual(report['implementation']['id'], self.rid)
        self.assertEqual(report['implementation']['mode'], 'preview')
        self.assertIsNone(report['implementation']['effective_quality_claim'])
        self.assertEqual((self.root / 'calls').read_text().splitlines(), ['call'])
        rel.verify(self.state, self.rid)  # Execution creates no untracked bytecode files.

    def test_released_trace_is_refused_inside_stable_checkout_before_backend_call(self):
        trace = self.source / 'private-trace'
        result = self.cli(['release','preview','--release',self.rid,'--',*self.review_args(trace)])
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn('outside the repository', result.stderr)
        self.assertFalse(trace.exists())
        self.assertFalse((self.root/'calls').exists())

    def test_workflow_privacy_guard_checks_both_checkout_and_bundle(self):
        import harness
        with mock.patch.object(sys, '_ringo_release', {'checkout_root':str(self.source)}, create=True):
            for target in (self.source/'private-work', rel.HERE/'private-work'):
                with self.subTest(target=str(target)), self.assertRaises(ValueError):
                    harness.outside_repo(target, 'workflow run')

    def test_benchmark_baseline_release_keeps_release_identity_and_selected_code(self):
        import bench_latency
        rid, config, command = bench_latency.baseline_setup(release_id=self.rid, state=self.state)
        self.assertEqual(rid, self.rid)
        self.assertEqual(config, self.manifest['default_review_sha256'])
        self.assertIn('preview', command)
        self.assertEqual(command[-2:], [self.rid, '--'])
        for args in ({}, {'baseline':self.source,'release_id':self.rid,'state':self.state},
                     {'release_id':self.rid}):
            with self.subTest(args=args), self.assertRaises(ValueError):
                bench_latency.baseline_setup(**args)

    def test_benchmark_can_select_the_before_version_bootstrap(self):
        import bench_latency
        launcher = self.source / 'ringo.py'
        rid, config, command = bench_latency.baseline_setup(
            release_id=self.rid, state=self.state, launcher=launcher)
        self.assertEqual(rid,self.rid)
        self.assertEqual(config,self.manifest['default_review_sha256'])
        self.assertEqual(command[:2],[sys.executable,str(launcher)])
        with self.assertRaises(ValueError):
            bench_latency.baseline_setup(baseline=self.source,launcher=launcher)
        with self.assertRaises(ValueError):
            bench_latency.baseline_setup(release_id=self.rid,state=self.state,launcher=self.root/'missing.py')

    def test_selection_benchmark_approval_is_explicitly_a_private_test_fixture(self):
        import bench_latency
        state = bench_latency.selection_fixture(self.state,self.rid,30)
        approval = rel.approved(self.state,state['active'])
        self.assertIn('NOT HUMAN ADOPTION',approval['approved_by'])
        self.assertEqual(approval['latency']['reports'][0]['measurement']['baseline'],'synthetic-test-setup')
        self.assertEqual(state['active']['release'],self.rid)

    def test_active_bundle_ignores_broken_checkout_runtime(self):
        self.activate()
        (self.source / 'harness.py').write_text('raise RuntimeError("wrong checkout imported")\n')
        trace = self.root / 'isolated'
        proc = self.cli(self.review_args(trace))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(json.loads((trace / 'implementation.json').read_text())['id'], self.rid)

    def test_explicit_pipeline_override_is_recorded_without_quality_claim(self):
        self.activate()
        override = self.root / 'override.json'
        override.write_text(json.dumps({'version': 1, 'kind': 'review', 'role': 'review',
            'prompts_dir': str(self.source / 'prompts'),
            'passes': [{'id': 'review', 'prompt': 'review.md', 'reply': 'findings'}]}))
        trace = self.root / 'override-trace'
        proc = self.cli([*self.review_args(trace), '--pipeline', str(override)])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        info = json.loads((trace / 'implementation.json').read_text())
        self.assertEqual(info['pipeline_override'], str(override))
        self.assertIsNone(info['effective_quality_claim'])

    def test_quality_claim_is_scoped_to_exact_backend_and_review_default(self):
        import harness
        plan_hash = rel.config_hash(pipelines.load_plan(None, 'review'))
        claim = {'kind':'rrsi','backend':'claude','checker_backend':None,'plan_sha256':plan_hash}
        material = self.root / 'quality-material.txt'
        material.write_text('fixture')
        for backend, kind, expected in [('claude','review',claim), ('codex-chatgpt','review',None),
                                         ('claude','design',None)]:
            err = io.StringIO()
            with mock.patch.object(rel, 'identity', return_value={'mode':'release','quality_claim':claim}), \
                 mock.patch.object(harness, 'run_plan', return_value=('No defects found.', {})), \
                 contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
                code = harness.main([kind,'--input',str(material),'--backend',backend])
            self.assertEqual(code, 0)
            summary = json.loads(err.getvalue().split('harness: ')[-1])
            self.assertEqual(summary['implementation']['effective_quality_claim'], expected)

    def test_inflight_run_remains_pinned_during_activation(self):
        self.activate()
        (self.root / 'wait').touch()
        trace = self.root / 'inflight'
        process = subprocess.Popen([sys.executable, str(self.source / 'ringo.py'), *self.review_args(trace)],
                                   env=self.env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 10
            while not (self.root / 'ready').exists() and time.monotonic() < deadline:
                time.sleep(.01)
            self.assertTrue((self.root / 'ready').exists())
            next_id = self.next_release()
            self.activate(next_id, self.rid)
            (self.root / 'go').touch()
            _, err = process.communicate(timeout=10)
            self.assertEqual(process.returncode, 0, err)
            self.assertEqual(json.loads((trace / 'implementation.json').read_text())['id'], self.rid)
            self.assertEqual(rel.pointer(self.state)['active']['release'], next_id)
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()


if __name__ == '__main__':
    unittest.main()
