"""Bounded revision, execution integrity and failure checks, using offline fixtures."""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import harness
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
                     'max_rounds': 3, 'max_seconds': 30, 'samples': 2}

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

    def test_calls_are_max_rounds_times_one_writer_plus_samples(self):
        path = self.root / 'task.json'
        path.write_text(json.dumps({**self.spec, 'max_rounds': 2, 'samples': 3}))
        drafts = iter(['def f(x): return x\n', 'def f(x): return 3*x\n'])

        def call(_backend, system, message, timeout_s):  # the council never recommends
            if system.startswith('Implement the task'):
                return json.dumps(self.proposal(next(drafts))), {'cost_usd': 0.01}
            return json.dumps({'accept': False}), {'cost_usd': 0.01}
        report = workflow.run(path, self.root / 'run', call_fn=call)
        self.assertEqual(report['status'], 'needs_revision')
        self.assertEqual(report['usage']['calls'], 2 * (1 + 3))
        self.assertIsNone(report['final'])

    def test_default_samples_match_the_council_default_of_six(self):
        spec = dict(self.spec)
        del spec['samples']
        path = self.root / 'task.json'
        path.write_text(json.dumps(spec))
        self.assertEqual(workflow.load_spec(path)['samples'], 6)
        report = workflow.run(path, self.root / 'run', call_fn=lambda _b, system, *a: (json.dumps(
            self.proposal('def f(x): return 2*x\n') if system.startswith('Implement the task') else {'accept': True}), {}))
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['usage']['calls'], 1 + 6)

    @staticmethod
    def edits(*triples, rationale='Targeted change.'):
        return {'edits': [{'path': p, 'old': o, 'new': n} for p, o, n in triples], 'rationale': rationale}

    def test_edit_reply_is_applied_as_complete_files_before_checks_and_judging(self):
        source = FAKE_BRIDGE.replace("'JUDGE '+str(n)", "'JUDGE '+r['proposal']")
        (self.root / 'bridge.py').write_text(source)
        bad = self.proposal('# keep\ndef f(x): return x\n')
        report, seen = self.execute([bad, self.edits(('value.py', 'return x', 'return 2*x'))])
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['usage']['calls'], 6)
        applied = '# keep\ndef f(x): return 2*x\n'
        self.assertEqual((self.root / 'run/final/value.py').read_text(), applied)
        self.assertEqual(json.loads(seen[4][1].removeprefix('JUDGE '))['files'], {'value.py': applied})
        saved = json.loads((self.root / 'run/round-2/proposal.json').read_text())
        self.assertEqual(saved['files'], {'value.py': applied})
        self.assertEqual(saved['edits'][0]['old'], 'return x')
        self.assertNotIn('edits', json.loads((self.root / 'run/round-1/proposal.json').read_text()))

    def test_failed_anchor_is_malformed_and_keeps_the_earlier_check_and_council_feedback(self):
        bad = self.proposal('def f(x): return x\n')
        miss = self.edits(('value.py', 'return 2*x', 'return x+x'))
        good = self.edits(('value.py', 'return x', 'return 2*x'))
        report, seen = self.execute([bad, miss, good])
        self.assertEqual([r['status'] for r in report['rounds']], ['revise', 'malformed', 'ready_for_review'])
        self.assertEqual(report['usage']['calls'], 3 + 1 + 3)  # The malformed round makes no judge calls.
        self.assertFalse((self.root / 'run/round-2/candidate').exists())
        error = report['rounds'][1]['feedback']['protocol_error']
        self.assertIn('edit 1 (value.py): old matches 0 places', error)
        self.assertIn("first 7 of 10 characters match at line 1: 'def f(x): return x'", error)
        third = json.loads(seen[4][1])
        self.assertEqual(third['files'], bad['files'])
        self.assertEqual(set(third['feedback']), {'checks', 'decision', 'scored', 'protocol_error'})
        self.assertEqual(third['feedback']['checks'][0]['status'], 'fail')

    def test_edit_validation_is_strict_and_all_or_nothing(self):
        current = {'a.txt': 'aaa\nxyz\n', 'b.txt': 'one two\n'}
        cases = {
            'old matches more than one place': self.edits(('a.txt', 'aa', 'b')),  # Overlapping matches count.
            'edits 1 and 2 overlap': self.edits(('a.txt', 'aaa', 'c'), ('a.txt', 'a\nx', 'd')),
            'edit 2 (a.txt): old matches 0 places': self.edits(('a.txt', 'xyz', 'NEW'), ('a.txt', 'NEW', 'z')),
            'edit 1 (b.txt): old matches 0 places': self.edits(('b.txt', 'zz', '')),
            'not a declared editable file': self.edits(('../a.txt', 'aaa', 'b')),
            'old is empty': self.edits(('b.txt', '', 'x')),
            'needs string path, old and new only': {'edits': [{'path': 'a.txt', 'old': 'x'}], 'rationale': 'x'},
            'not both': {**self.edits(('a.txt', 'xyz', 'q')), 'files': current},
            'exactly the declared editable files': {'files': {'a.txt': 'x'}, 'rationale': 'x'},
            'oversized file: a.txt': self.edits(('a.txt', 'xyz', 'x' * (workflow.LIMIT + 1))),
            'edits must be a list': {'edits': {}, 'rationale': 'x'},
        }
        for message, reply in cases.items():
            with self.subTest(message=message), self.assertRaises(ValueError) as caught:
                workflow.materialize(current, current, reply)
            self.assertIn(message, str(caught.exception))
        self.assertEqual(current, {'a.txt': 'aaa\nxyz\n', 'b.txt': 'one two\n'})
        files = workflow.materialize(current, current, self.edits(('a.txt', 'xyz', 'XYZ'), ('a.txt', 'aaa', 'A')))
        self.assertEqual(files, {'a.txt': 'A\nXYZ\n', 'b.txt': 'one two\n'})  # Order-independent; b untouched.

    def test_unchanged_reply_ends_the_run_in_any_round(self):
        report, _ = self.execute([{'edits': [], 'rationale': 'Nothing to change.'}])
        self.assertEqual(report['status'], 'repeated_candidate')
        self.assertEqual(report['usage']['calls'], 1)
        shutil.rmtree(self.root / 'run')
        report, _ = self.execute([self.proposal('')])  # A full-file echo of the shown files.
        self.assertEqual(report['status'], 'repeated_candidate')
        self.assertEqual(report['usage']['calls'], 1)
        shutil.rmtree(self.root / 'run')
        bad = self.proposal('def f(x): return x')
        report, _ = self.execute([bad, self.edits(('value.py', 'x)', 'y)')), self.edits(('value.py', 'y)', 'x)'))])
        self.assertEqual(report['status'], 'repeated_candidate')
        self.assertEqual(report['usage']['calls'], 3 + 3 + 1)

    def test_edit_that_makes_a_check_mutate_a_fixture_still_stops(self):
        first = self.proposal('def f(x): return x\n')
        mutate = self.edits(('value.py', 'def f', "from pathlib import Path\nPath('check.py').write_text('pass')\ndef f"))
        report, _ = self.execute([first, mutate])
        self.assertEqual(report['status'], 'error')
        self.assertIn('mutated', report['error'])
        self.assertIsNone(report['final'])

    def test_an_old_spec_with_max_calls_is_refused_clearly(self):
        path = self.root / 'task.json'
        path.write_text(json.dumps({**self.spec, 'max_calls': 9}))
        with self.assertRaisesRegex(ValueError, 'max_calls was removed: calls are set by max_rounds and samples'):
            workflow.load_spec(path)

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

    def test_deeply_nested_writer_json_becomes_protocol_feedback(self):
        depth = 200000  # Also exceeds Python 3.14's JSON parser nesting limit.
        report, seen = self.execute(['[' * depth + '0' + ']' * depth,
                                     self.proposal('def f(x): return 2*x')])
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['rounds'][0]['status'], 'malformed')
        self.assertIn('nesting', seen[1][1])

    def test_invalid_backend_commands_and_missing_cost_fallback_are_rejected_at_setup(self):
        for change in ({'writer': 'claude:'}, {'judge': 'ollama:'}, {'max_cost': 1}):
            path = self.root / 'task.json'
            path.write_text(json.dumps({**self.spec, **change}))
            with self.subTest(change=change), self.assertRaises(ValueError):
                workflow.load_spec(path)

    def test_invalid_bridge_fails_before_any_provider_call(self):
        self.spec['council'] = [sys.executable, str(self.root / 'missing-bridge.py')]
        report, seen = self.execute([])
        self.assertEqual(report['status'], 'error')
        self.assertEqual(report['usage']['calls'], 0)
        self.assertEqual(seen, [])
        self.assertEqual(report['rounds'], [])

    def test_expired_deadline_during_call_is_a_budget_stop_but_early_timeout_is_an_error(self):
        path = self.root / 'task.json'
        path.write_text(json.dumps(self.spec))
        prepared = {'version': 1, 'fingerprint': 'fixed', 'policy_sha256': 'a' * 64,
                    'requests': ['one', 'two']}
        for expired in (False, True):
            clock = [100.0]
            def call(*args):
                clock[0] = 131.0 if expired else 101.0
                raise subprocess.TimeoutExpired('provider', 1)
            with self.subTest(expired=expired), mock.patch.object(workflow, 'bridge', return_value=prepared), \
                    mock.patch.object(workflow.time, 'monotonic', side_effect=lambda: clock[0]):
                report = workflow.run(path, self.root / f'run-{expired}', call_fn=call)
            self.assertEqual(report['status'], 'budget_exhausted' if expired else 'error')
            self.assertEqual(report['usage']['calls'], 1)
            self.assertEqual(report['usage']['unpriced_calls'], 1)

    def test_failed_call_keeps_its_raw_output_in_the_trace(self):
        def call(*args):
            raise harness.CallError('codex-chatgpt turn rejected: no final agent message', None, '{"type":"turn.failed"}')
        path = self.root / 'task.json'
        path.write_text(json.dumps(self.spec))
        report = workflow.run(path, self.root / 'run', call_fn=call)
        self.assertEqual(report['status'], 'error')
        self.assertEqual(report['usage']['untokened_calls'], 1)
        raws = list((self.root / 'run' / 'steps').glob('*.raw.txt'))
        self.assertEqual([r.read_text() for r in raws], ['{"type":"turn.failed"}'])

    def test_invalid_real_council_brief_fails_before_inference(self):
        if not os.environ.get('RINGO_COUNCIL_ROOT'):
            self.skipTest('set RINGO_COUNCIL_ROOT for real bridge smoke')
        council_root = Path(os.environ['RINGO_COUNCIL_ROOT']).resolve()
        self.spec.update(council=[sys.executable, str(council_root / 'bridge.py')],
                         rubric=str(council_root / 'rubrics/code_change.json'))
        report, seen = self.execute([], brief='Missing a hard constraints section')
        self.assertEqual(report['status'], 'error')
        self.assertEqual(report['usage']['calls'], 0)
        self.assertEqual(seen, [])

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
