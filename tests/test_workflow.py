"""Bounded revision, execution integrity and failure checks, using offline fixtures."""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
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
        self.assertEqual(report['usage']['calls'], 4)
        self.assertEqual([r['checks'][0]['status'] for r in report['rounds']], ['fail', 'pass'])
        self.assertEqual(report['rounds'][0]['assessment_skipped'], 'checks_failed')
        self.assertNotIn('decision', report['rounds'][0])
        self.assertIn('fail', seen[1][1])
        self.assertEqual((self.root / 'run/final/value.py').read_text(), good['files']['value.py'])
        self.assertTrue(json.loads((self.root / 'run/shortcomings.json').read_text())['development_only'])

    def test_calls_are_max_rounds_times_one_writer_plus_samples(self):
        path = self.root / 'task.json'
        path.write_text(json.dumps({**self.spec, 'max_rounds': 2, 'samples': 3}))
        drafts = iter(['def f(x): return 2*x\n', 'def f(x): return x+x\n'])

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
        self.assertEqual(report['usage']['calls'], 1 + 3)  # three agreeing judges already recommend
        self.assertEqual((report['council_judges'], report['council_samples']), (3, 6))
        self.assertEqual(report['rounds'][0]['council_stages'], [{'judges': 3, 'action': 'recommend'}])

    @staticmethod
    def edits(*triples, rationale='Targeted change.'):
        return {'edits': [{'path': p, 'old': o, 'new': n} for p, o, n in triples], 'rationale': rationale}

    def test_edit_reply_is_applied_as_complete_files_before_checks_and_judging(self):
        source = FAKE_BRIDGE.replace("'JUDGE '+str(n)", "'JUDGE '+r['proposal']")
        (self.root / 'bridge.py').write_text(source)
        bad = self.proposal('# keep\ndef f(x): return x\n')
        report, seen = self.execute([bad, self.edits(('value.py', 'return x', 'return 2*x'))])
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['usage']['calls'], 4)
        applied = '# keep\ndef f(x): return 2*x\n'
        self.assertEqual((self.root / 'run/final/value.py').read_text(), applied)
        self.assertEqual(json.loads(seen[2][1].removeprefix('JUDGE '))['files'], {'value.py': applied})
        saved = json.loads((self.root / 'run/round-2/proposal.json').read_text())
        self.assertEqual(saved['files'], {'value.py': applied})
        self.assertEqual(saved['edits'][0]['old'], 'return x')
        self.assertNotIn('edits', json.loads((self.root / 'run/round-1/proposal.json').read_text()))

    def test_failed_anchor_is_malformed_and_keeps_the_earlier_check_feedback(self):
        bad = self.proposal('def f(x): return x\n')
        miss = self.edits(('value.py', 'return 2*x', 'return x+x'))
        good = self.edits(('value.py', 'return x', 'return 2*x'))
        report, seen = self.execute([bad, miss, good])
        self.assertEqual([r['status'] for r in report['rounds']], ['revise', 'malformed', 'ready_for_review'])
        self.assertEqual(report['usage']['calls'], 1 + 1 + 3)  # Failed/malformed rounds make no judge calls.
        self.assertFalse((self.root / 'run/round-2/candidate').exists())
        error = report['rounds'][1]['feedback']['protocol_error']
        self.assertIn('edit 1 (value.py): old matches 0 places', error)
        self.assertIn("first 7 of 10 characters match at line 1: 'def f(x): return x'", error)
        third = json.loads(seen[2][1])
        self.assertEqual(third['files'], bad['files'])
        self.assertEqual(set(third['feedback']), {'checks', 'protocol_error'})
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
        self.assertEqual(report['usage']['calls'], 1 + 1 + 1)

    def test_edit_that_makes_a_check_mutate_a_fixture_still_stops(self):
        first = self.proposal('def f(x): return x\n')
        mutate = self.edits(('value.py', 'def f', "from pathlib import Path\nPath('check.py').write_text('pass')\ndef f"))
        report, _ = self.execute([first, mutate])
        self.assertEqual(report['status'], 'error')
        self.assertIn('mutated', report['error'])
        self.assertIsNone(report['final'])

    FINDING = ('What this change does: adds f.\n\n### 1. Floats are truncated (`value.py:1`)\n'
               '- Code: `return int(2*x)`\n- Problem: f(0.5) returns 1, should return 1.0.\n- Fix: return 2*x.\n')

    def work(self, writer, reviews, **changes):
        """A default (review) run: no council bridge, rubric or samples in the spec."""
        spec = {k: v for k, v in {**self.spec, **changes}.items() if k not in ('council', 'rubric', 'samples')}
        path = self.root / 'task.json'
        path.write_text(json.dumps(spec))
        seen, writer, reviews = [], iter(writer), iter(reviews)

        def call(_backend, system, message, timeout_s):
            seen.append((system, message))
            if system.startswith('Implement the task'):
                return json.dumps(next(writer)), {'cost_usd': 0.01}
            return next(reviews), {'cost_usd': 0.01}
        return workflow.run(path, self.root / 'run', call_fn=call), seen

    def test_default_reviews_a_passing_candidate_once_and_its_findings_drive_one_revision(self):
        first = self.proposal('def f(x): return int(2*x)\n')
        fixed = self.proposal('def f(x): return 2*x\n')
        report, seen = self.work([first, fixed], [self.FINDING])
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['assessment'], 'review')
        self.assertEqual(report['usage']['calls'], 3)  # writer, review, writer: the revision is not reviewed again
        self.assertEqual([r['status'] for r in report['rounds']], ['revise', 'ready_for_review'])
        self.assertEqual(report['rounds'][0]['review_findings'], 1)
        self.assertNotIn('review_findings', report['rounds'][1])
        system, message = seen[1]
        self.assertTrue(system.startswith('You review code changes.'))
        for evidence in (self.spec['brief'], 'value.py (after the change)', 'int(2*x)', 'check.py (read-only fixture)',
                         'observed check: reference (pass)', 'Author rationale: Implement the exact function'):
            self.assertIn(evidence, system)
        self.assertIn('Review the change in the material', message)
        feedback = json.loads(seen[2][1])['feedback']
        self.assertEqual(feedback['review'], self.FINDING.strip())
        self.assertEqual(feedback['checks'][0]['status'], 'pass')
        self.assertEqual(Path(report['review']).read_text(), self.FINDING.strip() + '\n')
        self.assertEqual((self.root / 'run/final/value.py').read_text(), fixed['files']['value.py'])
        self.assertFalse((self.root / 'run/round-1/council.json').exists())

    def test_a_review_without_findings_delivers_the_reviewed_candidate(self):
        good = self.proposal('def f(x): return 2*x\n')
        report, _ = self.work([good], ['What this change does: adds f.\n\nNo defects found.'])
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['usage']['calls'], 2)
        self.assertEqual(report['rounds'][0]['review_findings'], 0)
        self.assertEqual((self.root / 'run/final/value.py').read_text(), good['files']['value.py'])

    def test_an_unreadable_review_counts_as_a_finding(self):
        self.assertEqual(workflow.review_findings('Looks fine, but x may overflow.'), 1)
        self.assertEqual(workflow.review_findings('Summary.\n\nNo defects found.'), 0)
        self.assertEqual(workflow.review_findings(self.FINDING + '\n### 2. Other (`a:1`)\n- Code: x\n'), 2)

    def test_failed_checks_after_the_review_keep_it_and_the_next_passing_candidate_ships(self):
        first = self.proposal('def f(x): return int(2*x)\n')
        broken = self.proposal('def f(x): return x\n')
        fixed = self.proposal('def f(x): return 2*x\n')
        report, seen = self.work([first, broken, fixed], [self.FINDING])
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['usage']['calls'], 4)  # one review only
        self.assertEqual([r['status'] for r in report['rounds']], ['revise', 'revise', 'ready_for_review'])
        feedback = json.loads(seen[3][1])['feedback']
        self.assertEqual(feedback['checks'][0]['status'], 'fail')
        self.assertEqual(feedback['previous_review'], self.FINDING.strip())

    def test_review_findings_with_no_round_left_publish_nothing(self):
        report, _ = self.work([self.proposal('def f(x): return int(2*x)\n')], [self.FINDING], max_rounds=1)
        self.assertEqual(report['status'], 'needs_revision')
        self.assertEqual(report['usage']['calls'], 2)
        self.assertIsNone(report['final'])
        self.assertFalse((self.root / 'run/final').exists())
        self.assertTrue(Path(report['review']).is_file())

    def test_a_failed_review_call_is_retried_once_then_ends_the_run_with_error(self):
        spec = {k: v for k, v in self.spec.items() if k not in ('council', 'rubric', 'samples')}
        path = self.root / 'task.json'
        path.write_text(json.dumps(spec))
        calls = []

        def call(_backend, system, message, timeout_s):
            calls.append(system[:20])
            if system.startswith('Implement the task'):
                return json.dumps(self.proposal('def f(x): return 2*x\n')), {'cost_usd': 0.01}
            raise harness.CallError('codex-chatgpt turn rejected', {'cost_usd': 0.01}, 'raw')
        report = workflow.run(path, self.root / 'run', call_fn=call)
        self.assertEqual(report['status'], 'error')
        self.assertIn('turn rejected', report['error'])
        self.assertEqual(report['usage']['calls'], 3)  # writer, review, review retry
        self.assertIsNone(report['final'])

    def test_a_review_past_the_deadline_is_refused_before_any_call(self):
        spec = {k: v for k, v in self.spec.items() if k not in ('council', 'rubric', 'samples')}
        path = self.root / 'task.json'
        path.write_text(json.dumps({**spec, 'max_seconds': 30}))
        clock = [100.0]

        def call(_backend, system, message, timeout_s):
            self.assertTrue(system.startswith('Implement the task'), 'no review call may start after the deadline')
            return json.dumps(self.proposal('def f(x): return 2*x\n')), {'cost_usd': 0.01}
        real = workflow.checks

        def checks(*args):  # the passing checks use up the rest of the 30 s
            observed = real(*args)
            clock[0] = 131.0
            return observed
        with mock.patch.object(workflow.time, 'monotonic', side_effect=lambda: clock[0]), \
                mock.patch.object(workflow, 'checks', side_effect=checks):
            report = workflow.run(path, self.root / 'run', call_fn=call)
        self.assertEqual(report['status'], 'budget_exhausted')
        self.assertEqual(report['usage']['calls'], 1)
        self.assertEqual(report['usage']['admission_refusals'], 1)
        self.assertEqual(report['usage']['unpriced_calls'], 0)

    def test_the_review_sees_a_diff_for_a_file_that_started_nonempty(self):
        spec = {'brief': 'b', 'files': {'a.py': 'x = 1\ny = 2\n', 'new.py': ''}, 'fixtures': {}}
        material = workflow.review_material(spec, {'a.py': 'x = 1\ny = 3\n', 'new.py': 'z = 0\n'}, 'r', [])
        self.assertIn('=== a.py (diff from the initial file) ===\n--- a/a.py\n+++ b/a.py\n', material)
        self.assertIn('-y = 2\n+y = 3\n', material)
        self.assertNotIn('new.py (diff', material)
        self.assertIn('=== new.py (after the change) ===\nz = 0\n', material)

    def test_a_rewritten_file_gets_no_diff_since_the_whole_file_follows(self):
        stub = 'def f(x):\n    raise NotImplementedError\n'
        body = 'def f(x):\n' + ''.join(f'    y{i} = x + {i}\n' for i in range(10)) + '    return y9\n'
        material = workflow.review_material({'brief': 'b', 'files': {'a.py': stub}, 'fixtures': {}}, {'a.py': body}, 'r', [])
        self.assertNotIn('a.py (diff', material)
        self.assertEqual(material.count('y9 = x + 9'), 1)

    def test_council_settings_without_a_council_bridge_are_refused(self):
        base = {k: v for k, v in self.spec.items() if k not in ('council', 'rubric', 'samples')}
        path = self.root / 'task.json'
        for extra in ({'rubric': 'r.json'}, {'samples': 6}, {'first_judges': 3}):
            path.write_text(json.dumps({**base, **extra}))
            with self.subTest(extra=extra), self.assertRaisesRegex(ValueError, 'configure the Council'):
                workflow.load_spec(path)
        path.write_text(json.dumps(base))
        spec = workflow.load_spec(path)
        self.assertNotIn('samples', spec)
        self.assertNotIn('rubric', spec)

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
        self.assertEqual(report['usage']['calls'], 2)
        self.assertIsNone(report['final'])

    def test_failed_last_round_never_spends_council_calls_or_publishes(self):
        report, seen = self.execute([self.proposal('def f(x): return x\n')], max_rounds=1, samples=6)
        self.assertEqual(report['status'], 'needs_revision')
        self.assertEqual(report['usage']['calls'], 1)
        self.assertEqual(len(seen), 1)
        self.assertIsNone(report['final'])
        self.assertFalse((self.root / 'run/round-1/council.json').exists())
        self.assertFalse((self.root / 'run/final').exists())
        feedback = json.loads((self.root / 'run/shortcomings.json').read_text())['feedback']
        self.assertEqual(feedback['checks'][0]['status'], 'fail')

    def test_six_sample_council_only_sees_corrected_candidate(self):
        bad = self.proposal('def f(x): return x\n')
        good = self.proposal('def f(x): return 2*x\n')
        report, seen = self.execute([bad, good], max_rounds=2, samples=6)
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['usage']['calls'], 2 + 3)
        self.assertEqual(sum(system.startswith('Implement the task') for system, _ in seen), 2)
        self.assertTrue(all(system.startswith('You are an independent council judge') for system, _ in seen[2:]))
        feedback = json.loads(seen[1][1])['feedback']
        self.assertEqual(feedback['checks'][0]['status'], 'fail')
        self.assertNotIn('decision', feedback)
        self.assertEqual((self.root / 'run/final/value.py').read_text(), good['files']['value.py'])

    STRICT_BRIDGE = '''import json, sys
r = json.load(sys.stdin)
if r['operation'] == 'prepare':
    out = {'version': 1, 'fingerprint': 'f' + str(r['samples']), 'policy_sha256': 'a' * 64,
           'requests': ['JUDGE ' + str(n) for n in range(r['samples'])]}
else:
    assert r['fingerprint'] == 'f' + str(r['samples']) and len(r['replies']) == r['samples']
    yes = sum(json.loads(x)['accept'] for x in r['replies'])
    ok = yes == r['samples'] if r['samples'] < 6 else yes >= 5
    out = {'version': 1, 'report': {'complete': True, 'scored': [],
           'decision': {'action': 'recommend' if ok else 'escalate', 'reasons': []}}}
print(json.dumps(out))
'''

    def judged(self, verdicts, **changes):
        (self.root / 'strict.py').write_text(self.STRICT_BRIDGE)
        path = self.root / 'task.json'
        path.write_text(json.dumps({**self.spec, 'council': [sys.executable, str(self.root / 'strict.py')],
                                    'max_rounds': 1, 'samples': 6, **changes}))
        verdicts, seen = iter(verdicts), []
        def call(_backend, system, message, timeout_s):
            if system.startswith('Implement the task'):
                return json.dumps(self.proposal('def f(x): return 2*x\n')), {}
            seen.append(message)
            return json.dumps({'accept': next(verdicts)}), {}
        return workflow.run(path, self.root / 'run', call_fn=call), seen

    def test_a_first_stage_that_does_not_recommend_calls_the_remaining_judges(self):
        report, seen = self.judged([False] + [True] * 5)
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(seen, [f'JUDGE {n}' for n in range(6)])  # the first three are the full gate's first three
        self.assertEqual(report['rounds'][0]['council_stages'],
                         [{'judges': 3, 'action': 'escalate'}, {'judges': 6, 'action': 'recommend'}])
        self.assertEqual((report['council_judges'], report['usage']['calls']), (6, 7))

    def test_first_judges_equal_to_samples_always_runs_the_full_gate(self):
        report, seen = self.judged([True] * 6, first_judges=6)
        self.assertEqual((len(seen), report['council_judges']), (6, 6))
        self.assertEqual(report['rounds'][0]['council_stages'], [{'judges': 6, 'action': 'recommend'}])

    def test_first_judges_must_lie_between_two_and_samples(self):
        for bad in (1, 7, 2.5, True):
            with self.subTest(bad=bad):
                path = self.root / 'task.json'
                path.write_text(json.dumps({**self.spec, 'samples': 6, 'first_judges': bad}))
                with self.assertRaises(ValueError):
                    workflow.load_spec(path)

    def test_a_council_whose_staged_requests_differ_is_refused(self):
        (self.root / 'shifty.py').write_text(self.STRICT_BRIDGE.replace("'JUDGE ' + str(n)", "'JUDGE ' + str(n) + str(r['samples'])"))
        path = self.root / 'task.json'
        path.write_text(json.dumps({**self.spec, 'council': [sys.executable, str(self.root / 'shifty.py')],
                                    'max_rounds': 1, 'samples': 6}))
        report = workflow.run(path, self.root / 'run', call_fn=lambda _b, system, *a: (json.dumps(
            self.proposal('def f(x): return 2*x\n') if system.startswith('Implement the task') else {'accept': True}), {}))
        self.assertEqual(report['status'], 'error')
        self.assertIn('same first judge requests', report['error'])
        self.assertEqual(report['usage']['calls'], 1)  # refused before any judge call

    def test_all_failed_candidates_use_only_the_declared_writer_rounds(self):
        report, seen = self.execute([self.proposal('def f(x): return '+expr+'\n')
                                     for expr in ('x', '3*x', '4*x')], max_rounds=3, samples=6)
        self.assertEqual(report['status'], 'needs_revision')
        self.assertEqual(report['usage']['calls'], 3)
        self.assertEqual(len(seen), 3)
        self.assertTrue(all(row['assessment_skipped'] == 'checks_failed' for row in report['rounds']))
        self.assertFalse((self.root / 'run/final').exists())

    def test_timed_out_check_drives_correction_without_judging_failed_candidate(self):
        self.spec['fixtures']['check.py'] += "\nif n['f'](5) == 10 and 'slow' in Path('value.py').read_text():\n    import time\n    time.sleep(3)\n"
        self.spec['checks'][0]['timeout_s'] = .3
        report, seen = self.execute([self.proposal('def f(x): return 2*x  # slow\n'),
                                     self.proposal('def f(x): return 2*x\n')], max_rounds=2)
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['usage']['calls'], 4)
        self.assertEqual(report['rounds'][0]['checks'][0]['status'], 'timeout')
        self.assertEqual(json.loads(seen[1][1])['feedback']['checks'][0]['status'], 'timeout')

    def test_prior_council_feedback_is_retained_and_bound_to_its_candidate(self):
        drafts = iter(['def f(x): return 2*x\n', 'def f(x): return x\n', 'def f(x): return x+x\n'])
        path = self.root / 'task.json'
        path.write_text(json.dumps(self.spec))
        writer_messages = []

        def call(_backend, system, message, timeout_s):
            if system.startswith('Implement the task'):
                writer_messages.append(json.loads(message))
                reply = self.proposal(next(drafts))
            else:
                reply = {'accept': len(writer_messages) == 3}
            return json.dumps(reply), {}

        report = workflow.run(path, self.root / 'run', call_fn=call)
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['usage']['calls'], 3 + 2 * self.spec['samples'])
        feedback = writer_messages[2]['feedback']
        self.assertEqual(feedback['checks'][0]['status'], 'fail')
        self.assertNotIn('decision', feedback)
        self.assertEqual(feedback['previous_council']['candidate_sha256'], report['rounds'][0]['candidate_sha256'])
        self.assertEqual(feedback['previous_council']['decision']['action'], 'escalate')
        self.assertNotEqual(feedback['previous_council']['candidate_sha256'], report['rounds'][1]['candidate_sha256'])

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

    def test_a_passing_check_cannot_leave_a_child_running(self):  # process group on POSIX, job object on Windows
        marker = self.root / 'ticks'
        ticker = f'import time\nfor _ in range(200):\n    open({str(marker)!r}, "a").write("x")\n    time.sleep(0.05)\n'
        starter = (f'import os, subprocess, sys, time\nsubprocess.Popen([sys.executable, "-c", {ticker!r}])\n'
                   f'while not os.path.exists({str(marker)!r}): time.sleep(0.01)\n')
        spec = {**self.spec, 'checks': [{'name': 'check', 'argv': [sys.executable, '-c', starter], 'timeout_s': 30}]}
        self.assertEqual(workflow.checks(spec, self.root, self.root, lambda: 60)[0]['status'], 'pass')
        ticks = marker.stat().st_size
        time.sleep(0.5)
        self.assertEqual(marker.stat().st_size, ticks)

    @unittest.skipUnless(os.name == 'posix', 'os.killpg and its macOS EPERM case exist only on POSIX')
    def test_cleanup_of_a_group_that_holds_only_zombies_does_not_fail_the_check(self):
        # macOS killpg raises EPERM, not ESRCH, when the check's group holds only unreaped zombies.
        spec = {**self.spec, 'checks': [{'name': 'check', 'argv': ['check'], 'timeout_s': 1}]}
        for error in (ProcessLookupError(), PermissionError(1, 'Operation not permitted')):
            proc = mock.Mock(pid=123)
            proc.wait.side_effect = [0, 0]
            proc.poll.return_value = 0  # the check itself has exited and been reaped
            with self.subTest(error=error), mock.patch.object(workflow.subprocess, 'Popen', return_value=proc), \
                    mock.patch('os.killpg', side_effect=error), mock.patch.object(workflow.os, 'name', 'posix'):
                self.assertEqual(workflow.checks(spec, self.root, self.root, lambda: 10)[0]['status'], 'pass')
                self.assertEqual(proc.wait.call_count, 2)

    @unittest.skipUnless(os.name == 'posix', 'os.killpg exists only on POSIX')
    def test_a_live_check_that_cannot_be_signalled_stops_instead_of_waiting_forever(self):
        spec = {**self.spec, 'checks': [{'name': 'check', 'argv': ['check'], 'timeout_s': 1}]}
        proc = mock.Mock(pid=123)
        proc.wait.side_effect = [subprocess.TimeoutExpired('check', 1)]  # a second wait would hang
        proc.poll.return_value = None
        with mock.patch.object(workflow.subprocess, 'Popen', return_value=proc), \
                mock.patch('os.killpg', side_effect=PermissionError(1, 'Operation not permitted')), \
                mock.patch.object(workflow.os, 'name', 'posix'), self.assertRaises(PermissionError):
            workflow.checks(spec, self.root, self.root, lambda: 10)
        self.assertEqual(proc.wait.call_count, 1)

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

    def test_real_bridge_gives_the_same_first_judge_requests_at_three_and_six_samples(self):
        if not os.environ.get('RINGO_COUNCIL_ROOT'):
            self.skipTest('set RINGO_COUNCIL_ROOT for real bridge smoke')
        council_root = Path(os.environ['RINGO_COUNCIL_ROOT']).resolve()
        rubric = str(council_root / 'rubrics/code_change.json')
        proposal = json.dumps({'files': {'value.py': 'def f(x): return 2*x'}, 'rationale': 'r'})
        prepared = {}
        for samples in (3, 6):
            out = workflow.bridge({'council': [sys.executable, str(council_root / 'bridge.py')]},
                                  {'operation': 'prepare', 'brief': self.spec['brief'], 'proposal': proposal,
                                   'rubric': rubric, 'samples': samples}, 60)
            prepared[samples] = out
        self.assertEqual(prepared[3]['requests'], prepared[6]['requests'][:3])
        self.assertEqual(prepared[3]['policy_sha256'], prepared[6]['policy_sha256'])
        self.assertNotEqual(prepared[3]['fingerprint'], prepared[6]['fingerprint'])


if __name__ == '__main__':
    unittest.main()
