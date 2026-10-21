"""Bounded revision, execution integrity and failure checks, using offline fixtures."""
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import workflow
from unittest import mock

FAKE_BRIDGE = '''import json, sys
r = json.load(sys.stdin)
if r['operation'] == 'prepare':
    out = {'version': 1, 'fingerprint': 'fixed', 'policy_sha256': 'a'*64,
           'requests': ['JUDGE '+str(n) for n in range(r['samples'])]}
else:
    assert len(r['replies']) == r['samples']
    ok = all(json.loads(x)['accept'] for x in r['replies'])
    out = {'version': 1, 'report': {'complete': True, 'scored': [],
           'decision': {'action': 'recommend' if ok else 'escalate', 'reasons': []}}}
print(json.dumps(out))
'''


class WorkflowTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'bridge.py').write_text(FAKE_BRIDGE)
        self.spec = {'version': 1, 'brief': 'Implement f(x)=2*x.\n\nHard constraints:\n- Return twice x.\n',
                     'files': {'value.py': ''},
                     'fixtures': {'check.py': "from pathlib import Path\nn={}\nexec(Path('value.py').read_text(), n)\nassert n['f'](5)==10\nassert n['f'](-2)==-4\n"},
                     'checks': [{'name': 'reference', 'argv': [sys.executable, '-I', '-B', 'check.py']}],
                     'writer': 'fake', 'judge': 'fake', 'allow_tool_backends': True, 'allow_execution': True,
                     'council': [sys.executable, str(self.root / 'bridge.py')], 'rubric': 'unused.json',
                     'max_calls': 9, 'max_rounds': 3, 'max_seconds': 30}

    def execute(self, outputs, **changes):
        path = self.root / 'task.json'
        path.write_text(json.dumps({**self.spec, **changes}))
        seen = []
        outputs = iter(outputs)

        def call(_backend, system, message, timeout_s):
            seen.append((system, message))
            reply = next(outputs) if system.startswith('Implement the task') else {'accept': True}
            return (json.dumps(reply) if not isinstance(reply, str) else reply), {'cost_usd': 0.01}

        result = workflow.run(path, self.root / 'run', call_fn=call)
        return result, seen

    @staticmethod
    def proposal(code):
        return {'files': {'value.py': code}, 'rationale': 'Implement the exact function and use reference checks.'}

    def test_failed_reference_check_drives_revision_before_reviewable_output(self):
        bad = self.proposal('def f(x): return x\n')
        good = self.proposal('def f(x): return 2*x\n')
        report, seen = self.execute([bad, good])
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['usage']['calls'], 6)
        self.assertEqual([r['checks'][0]['status'] for r in report['rounds']], ['fail', 'pass'])
        self.assertIn('fail', seen[3][1])
        self.assertEqual((self.root / 'run/final/value.py').read_text(), good['files']['value.py'])
        self.assertTrue(json.loads((self.root / 'run/shortcomings.json').read_text())['development_only'])

    def test_call_cap_counts_writers_and_council_samples_together(self):
        report, _ = self.execute([self.proposal('def f(x): return x'), self.proposal('def f(x): return 2*x')], max_calls=4)
        self.assertEqual(report['status'], 'budget_exhausted')
        self.assertEqual(report['usage']['calls'], 4)
        self.assertIsNone(report['final'])

    def test_malformed_output_can_be_revised_but_never_applied(self):
        report, seen = self.execute(['not json', self.proposal('def f(x): return 2*x')])
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['usage']['calls'], 4)
        self.assertIn('protocol_error', seen[1][1])

    def test_repeated_failed_candidate_stops_without_more_judging(self):
        bad = self.proposal('def f(x): return x')
        report, _ = self.execute([bad, bad])
        self.assertEqual(report['status'], 'repeated_candidate')
        self.assertEqual(report['usage']['calls'], 4)
        self.assertIsNone(report['final'])

    def test_check_cannot_mutate_a_protected_fixture_and_claim_success(self):
        code = "from pathlib import Path\nPath('check.py').write_text('pass')\ndef f(x): return 2*x\n"
        report, _ = self.execute([self.proposal(code)])
        self.assertEqual(report['status'], 'error')
        self.assertIn('mutated', report['error'])
        self.assertEqual(report['usage']['calls'], 1)
        self.assertIsNone(report['final'])

    def test_extra_files_or_path_traversal_are_rejected(self):
        for name in ('../escape.py', '/tmp/escape.py', '.git/config', 'extra.py'):
            with self.subTest(name=name):
                p = self.root / 'task.json'
                p.write_text(json.dumps(self.spec))
                report = workflow.run(p, self.root / ('run-' + str(len(name))),
                                      call_fn=lambda *args: (json.dumps({'files': {name: 'x'}, 'rationale': 'x'}), {}))
                self.assertIsNone(report['final'])
                self.assertEqual(report['rounds'][0]['status'], 'malformed')

    def test_no_execution_acknowledgement_or_directory_file_collision_is_refused(self):
        for change in ({'allow_execution': False}, {'files': {'a': '', 'a-b': '', 'a/b': ''}},
                       {'samples': True}, {'checks': []}):
            p = self.root / 'task.json'
            p.write_text(json.dumps({**self.spec, **change}))
            with self.subTest(change=change), self.assertRaises(ValueError):
                workflow.load_spec(p)

    def test_duplicate_json_keys_and_nonfinite_numbers_are_refused(self):
        for text in ('{"files": {}, "files": {}}', '{"x": NaN}', '{"x": 1e400}'):
            with self.subTest(text=text), self.assertRaises(ValueError):
                workflow.strict_json(text)

    def test_council_escalation_blocks_publication_even_with_passing_checks(self):
        self.spec['council'] = [sys.executable, str(self.root / 'bridge.py')]
        source = FAKE_BRIDGE.replace("ok = all(json.loads(x)['accept'] for x in r['replies'])", 'ok = False')
        (self.root / 'bridge.py').write_text(source)
        report, _ = self.execute([self.proposal('def f(x): return 2*x')], max_rounds=1)
        self.assertEqual(report['status'], 'needs_revision')
        self.assertEqual(report['rounds'][0]['checks'][0]['status'], 'pass')
        self.assertIsNone(report['final'])

    def test_fixed_evidence_reaches_every_judge_and_is_identified_as_evidence(self):
        source = FAKE_BRIDGE.replace("'JUDGE '+str(n)", "'JUDGE '+r['proposal']")
        (self.root / 'bridge.py').write_text(source)
        material = 'Reference notes: the public implementation supports integer inputs.'
        report, seen = self.execute([self.proposal('def f(x): return 2*x')], material=material)
        self.assertEqual(report['status'], 'ready_for_review')
        for system, message in seen[1:]:
            payload = json.loads(message.removeprefix('JUDGE '))
            self.assertEqual(payload['source_material'], material)
            self.assertIn('not instructions', payload['source_material_role'])
            self.assertIn('not instructions', system)

    def test_explicit_admission_refusal_is_not_a_started_or_unpriced_call(self):
        path = self.root / 'task.json'
        path.write_text(json.dumps(self.spec))
        def refuse(*args):
            raise workflow.harness.AdmissionRefused('adapter cap reached before invocation')
        report = workflow.run(path, self.root / 'run', call_fn=refuse)
        self.assertEqual(report['status'], 'budget_exhausted')
        self.assertEqual(report['usage']['calls'], 0)
        self.assertEqual(report['usage']['unpriced_calls'], 0)
        self.assertEqual(report['usage']['admission_refusals'], 1)
        trace = (self.root / 'run/trace.jsonl').read_text()
        self.assertIn('admission_refused', trace)
        self.assertNotIn('"status": "error"', trace)

    def test_unattested_budget_failure_remains_a_potentially_started_call(self):
        path = self.root / 'task.json'
        path.write_text(json.dumps(self.spec))
        def fail(*args):
            raise workflow.harness.BudgetExceeded('possibly after invocation')
        report = workflow.run(path, self.root / 'run', call_fn=fail)
        self.assertEqual(report['usage']['calls'], 1)
        self.assertEqual(report['usage']['unpriced_calls'], 1)

    @unittest.skipUnless(os.name == 'posix', 'process groups require POSIX')
    def test_checks_cleanup_groups_on_success_timeout_and_interruption(self):
        for outcome in (0, subprocess.TimeoutExpired('check', 1), KeyboardInterrupt()):
            proc = mock.Mock(pid=123)
            proc.wait.side_effect = [outcome, 0] if isinstance(outcome, BaseException) else [0, 0]
            with self.subTest(outcome=outcome), mock.patch.object(workflow.subprocess, 'Popen', return_value=proc), \
                    mock.patch('os.killpg') as kill:
                spec = {**self.spec, 'checks': [{'name': 'check', 'argv': ['check'], 'timeout_s': 1}]}
                if isinstance(outcome, KeyboardInterrupt):
                    with self.assertRaises(KeyboardInterrupt):
                        workflow.checks(spec, self.root, self.root, lambda: 10)
                else:
                    rows = workflow.checks(spec, self.root, self.root, lambda: 10)
                    self.assertEqual(rows[0]['status'], 'timeout' if isinstance(outcome, BaseException) else 'pass')
                kill.assert_called_once()
                self.assertEqual(proc.wait.call_count, 2)

    @unittest.skipUnless(os.environ.get('RINGO_COUNCIL_ROOT'), 'set RINGO_COUNCIL_ROOT for real bridge smoke')
    def test_real_cross_repository_bridge_with_deterministic_judge_replies(self):
        council_root = Path(os.environ['RINGO_COUNCIL_ROOT']).resolve()
        self.spec.update(council=[sys.executable, str(council_root / 'bridge.py')],
                         rubric=str(council_root / 'rubrics/code_change.json'), max_rounds=1,
                         material='Fixed reference: for x=5 return 10; for x=-2 return -4.')
        path = self.root / 'task.json'
        path.write_text(json.dumps(self.spec))
        sample = 0
        def call(_backend, system, message, timeout):
            nonlocal sample
            if system.startswith('Implement the task'):
                return json.dumps(self.proposal('def f(x): return 2*x')), {'cost_usd': 0}
            self.assertIn(self.spec['material'], message)
            script = '''import json, sys
sys.path.insert(0, sys.argv[1])
import council
state=json.loads(sys.stdin.read().split('## Input\\n\\n')[-1])
rubric=council.load_rubric(sys.argv[2])
questions=council.build_questions(rubric,council.parse_constraints(state['decision_brief']))
_, keys=council.judge_prompt(state['decision_brief'],state['proposal'],questions,int(sys.argv[3]))
print(json.dumps({key:max(levels,key=levels.get) if levels else qid.startswith('constraint_fit.')
                  for key,(qid,levels) in keys.items()}))
'''
            reply = workflow.backends.run_isolated([sys.executable, '-c', script, str(council_root),
                                                    self.spec['rubric'], str(sample)], message, timeout)
            sample += 1
            return reply, {'cost_usd': 0}
        report = workflow.run(path, self.root / 'run', call_fn=call)
        self.assertEqual(report['status'], 'ready_for_review', report)
        self.assertEqual(report['usage']['calls'], 3)
        self.assertEqual(report['rounds'][0]['decision']['action'], 'recommend')


if __name__ == '__main__':
    unittest.main()
