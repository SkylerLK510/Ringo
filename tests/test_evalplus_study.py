"""The EvalPlus study's offline parts: specs, hidden-test grading rules, seeded defects and delivered files."""
import json
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import evalplus_study as study
import workflow

TASK = {'task_id': 'HumanEval/900', 'entry_point': 'largest',
        'prompt': 'def largest(xs):\n    """Return the largest even number in xs, or None."""\n',
        'canonical_solution': '    evens = [x for x in xs if x % 2 == 0]\n    return max(evens) if evens else None\n',
        'test': 'def check(candidate):\n    assert candidate([1, 3]) is None\n',  # weak, as public checks are
        'base_input': [[[1, 3]]], 'plus_input': [[[3, 5]], [[-2, -8]], [[6]]], 'atol': '0'}
GOOD = TASK['prompt'] + '    # keep evens only\n    evens = [x for x in xs if x % 2 == 0]  # "a < b" stays\n' \
                       '    return max(evens) if evens else None\n'


class GradeTest(unittest.TestCase):
    def test_the_reference_passes_and_a_check_missed_defect_fails(self):
        self.assertTrue(study.grade_here(TASK, GOOD))
        self.assertFalse(study.grade_here(TASK, GOOD.replace('max(evens)', 'min(evens)')))
        self.assertFalse(study.grade_here(TASK, TASK['prompt'] + '    print("PASS")\n    raise SystemExit\n'))

    def test_floats_get_evalplus_tolerance_and_types_must_match(self):
        self.assertTrue(study.same(0.1 + 0.2, 0.3, 0, 'f', []))
        self.assertTrue(study.same([1.0, 2.0000001], [1.0, 2.0], 0, 'f', []))
        self.assertFalse(study.same(1, 1.5, 0, 'f', []))
        self.assertFalse(study.same((1.0,), [1.0], 1e-6, 'f', []))
        self.assertTrue(study.same(1.00004, 0.0, 1e-4, 'find_zero', [[0.0, 1.0]]) is False)
        self.assertTrue(study.same(0.00001, 99.0, 1e-4, 'find_zero', [[0.0, 1.0]]))  # a root, whatever the reference


class SeedTest(unittest.TestCase):
    def test_mutants_change_one_code_token_and_keep_comments_and_strings(self):
        found = dict(study.mutants(GOOD))
        self.assertTrue(any('max -> min' in where for where in found))
        for where, source in found.items():
            self.assertIn('# keep evens only', source, where)
            self.assertIn('"a < b" stays', source, where)
            self.assertIn('def largest(xs):', source)
        self.assertNotIn('Return the smallest', ''.join(found.values()))

    def test_a_seeded_defect_passes_the_public_check_and_fails_the_hidden_tests(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'task.json'
            path.write_text(json.dumps(study.task_spec(TASK, study.HERE / 'examples' / 'v3-codex.json')))
            spec = workflow.load_spec(path)
            survivors = [w for w, s in study.mutants(GOOD) if study.base_passes(spec, s) and not study.grade_here(TASK, s)]
        self.assertTrue(survivors)


class SpecTest(unittest.TestCase):
    def test_the_public_check_sees_helpers_the_prompt_defines(self):
        task = {**TASK, 'prompt': 'def helper(x):\n    return x\n\n\n' + TASK['prompt'],
                'test': 'def check(candidate):\n    assert candidate([helper(2)]) == 2\n'}
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'task.json'
            path.write_text(json.dumps(study.task_spec(task, study.HERE / 'examples' / 'v3-codex.json')))
            spec = workflow.load_spec(path)
            self.assertTrue(study.base_passes(spec, task['prompt'] + GOOD[len(TASK['prompt']):]))

    def test_a_task_is_a_v3_work_spec(self):
        spec = study.task_spec(TASK, study.HERE / 'examples' / 'v3-codex.json')
        self.assertNotIn('stop_on_checks', spec)
        self.assertEqual(spec['files'], {'solution.py': TASK['prompt']})
        self.assertIn('check(largest)', spec['fixtures']['check_base.py'])
        self.assertNotIn('plus_input', json.dumps(spec))  # hidden inputs never enter the spec
        self.assertEqual(spec['roles']['writer'], {'effort': 'low'})
        self.assertNotIn('roles', study.task_spec(TASK, study.HERE / 'examples' / 'v3-codex.json', 'claude'))

    def test_a_backend_that_can_read_files_is_refused(self):
        with self.assertRaisesRegex(ValueError, 'tool-free'):
            study.task_spec(TASK, study.HERE / 'examples' / 'v3-codex.json', 'codex')


class DeliveredTest(unittest.TestCase):
    def test_final_else_the_last_round_that_passed_its_checks(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            for n in (1, 2):
                (run / f'round-{n}/candidate').mkdir(parents=True)
            rounds = [{'round': 1, 'checks': [{'status': 'pass'}]}, {'round': 2, 'checks': [{'status': 'fail'}]}]
            (run / 'report.json').write_text(json.dumps({'rounds': rounds}))
            self.assertEqual(study.delivered(run), run / 'round-1/candidate')
            (run / 'final').mkdir()
            self.assertEqual(study.delivered(run), run / 'final')


class ReviewFindingTest(unittest.TestCase):
    """The Ringo review's four findings: hidden data, stopped runs in the harm bound, resume and the seed limit."""

    def test_a_draft_that_touches_files_or_processes_is_flagged(self):
        for code in ("import os\n", "from pathlib import Path\n", "x = open('hidden.json').read()\n", "p = __file__\n",
                     "__import__('subprocess')\n", "def f(:\n"):
            self.assertTrue(study.touches_files(code), code)
        self.assertFalse(study.touches_files(GOOD))
        self.assertFalse(study.touches_files("import math, hashlib\nopen_count = 'open'\n"))

    def test_resume_retries_runs_that_ended_in_an_error_and_keeps_results(self):
        with tempfile.TemporaryDirectory() as tmp:
            runs = Path(tmp)
            for name, status in (('HumanEval-1', 'ready_for_review'), ('HumanEval-2', 'error'),
                                 ('HumanEval-4', 'needs_revision')):
                (runs / name).mkdir()
                (runs / name / 'report.json').write_text(json.dumps({'status': status}))
            (runs / 'HumanEval-3').mkdir()  # interrupted before its report
            ids = [f'HumanEval/{i}' for i in range(5)]
            self.assertEqual(study.pending(runs, ids), ['HumanEval/0', 'HumanEval/2', 'HumanEval/3'])
            self.assertTrue((runs / 'HumanEval-2.incomplete-0').is_dir())
            self.assertFalse((runs / 'HumanEval-2').exists())
            self.assertTrue((runs / 'HumanEval-4').is_dir())  # a task outcome, not a failed run

    def test_stopped_and_suspect_runs_stay_out_of_the_harm_bound(self):
        import shadow_labels
        base = {'regime': 'r', 'implementation': 'x', 'artifact_kind': 'code', 'eligible': True, 'stopped': False,
                'identical': False, 'round1_tokens': 1, 'after_tokens': 1, 'avoided': False}
        rows = [{**base, 'ran_suffix': True, 'harm': True, 'suspect': False},
                *[{**base, 'ran_suffix': False, 'stopped': True, 'identical': None, 'harm': False}] * 99,
                {**base, 'ran_suffix': True, 'harm': False, 'suspect': True}]
        text = shadow_labels.summary(rows, .05)
        self.assertIn('graded harm: 1/1', text)
        self.assertIn('1 graded runs left out', text)

    def test_run_all_launches_every_pending_task_and_stops_after_three_failed_runs(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            (out / 'runs').mkdir()
            calls = []

            def fake(argv, **kw):
                calls.append(argv[argv.index('--out') + 1])
                status = 'ready_for_review' if len(calls) <= 2 else 'error'
                return unittest.mock.Mock(stdout=json.dumps({'status': status}), stderr='')
            ids = [f'HumanEval/{i}' for i in range(8)]
            with unittest.mock.patch.object(study.subprocess, 'run', side_effect=fake):
                study.run_all(out, ids, 1)
            self.assertEqual(len(calls), 5)  # two successes, then three failures in a row stop new launches

    def test_a_flagged_run_counts_toward_neither_bound(self):
        import shadow_labels
        base = {'regime': 'r', 'implementation': 'x', 'artifact_kind': 'code', 'eligible': True,
                'round1_tokens': 1, 'after_tokens': 1, 'avoided': False, 'harm': False}
        rows = [{**base, 'identical': False, 'suspect': False}, *[{**base, 'identical': True, 'suspect': True}] * 50]
        text = shadow_labels.summary(rows, .05)
        self.assertIn('change rate after round 1: 1/1', text)
        self.assertIn('graded harm: 0/1', text)

    def test_seeding_stops_at_the_limit_when_results_already_exist(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            for i in range(3):
                run = out / 'runs' / f'HumanEval-{i}'
                run.mkdir(parents=True)
                (run / 'grade.json').write_text(json.dumps({'round1_pass': True, 'delivered_pass': True}))
                (run / 'report.json').write_text(json.dumps({'rounds': [{'checks': [{'status': 'pass'}]}]}))
                (out / 'seeded' / run.name).mkdir(parents=True)
                (out / 'seeded' / run.name / 'solution.py').write_text(GOOD)
                (out / 'seeded' / run.name / 'result.json').write_text('{}')
            with unittest.mock.patch.object(study, 'dataset', return_value={}), \
                    unittest.mock.patch.object(study.subprocess, 'run') as launched:
                study.seed_all(out, 2, 1)
            launched.assert_not_called()  # two results exist: nothing new is reviewed
            with unittest.mock.patch.object(study, 'dataset', return_value={}), \
                    unittest.mock.patch.object(study.subprocess, 'run') as launched:
                study.seed_all(out, 2, 1, reviewer='claude')
            argvs = [c.args[0] for c in launched.call_args_list]
            # another reviewer reviews the same two seeded drafts and the same drafts before seeding: four calls
            self.assertEqual(len(argvs), 4)
            self.assertTrue(all(a[a.index('--reviewer') + 1] == 'claude' for a in argvs))
            self.assertEqual(sum('--clean' in a for a in argvs), 2)

    def test_the_report_compares_reviewers_on_the_same_drafts(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            usage = {'input': 100, 'output': 10}
            for i, (default, other, clean) in enumerate([(1, 1, 0), (0, 1, 1), (1, 0, 0)]):
                folder = out / 'seeded' / f'HumanEval-{i}'
                (folder / 'claude').mkdir(parents=True)
                (folder / 'result.json').write_text(json.dumps(
                    {'findings': default, 'mentions_change': bool(default), 'original_findings': 0, 'usage': usage}))
                (folder / 'claude' / 'result.json').write_text(json.dumps(
                    {'reviewer': 'claude', 'findings': other, 'mentions_change': False, 'usage': usage}))
                (folder / 'claude' / 'clean.json').write_text(json.dumps(
                    {'reviewer': 'claude', 'findings': clean, 'mentions_change': False, 'usage': usage}))
            with unittest.mock.patch('sys.stdout', new_callable=__import__('io').StringIO) as printed:
                study.report_all(out, .05)
            text = printed.getvalue()
        self.assertIn('seeded reviews (template judge): 2/3 raised a finding', text)
        self.assertIn('2/3 quoted the changed line; 330 tokens', text)
        self.assertIn('seeded reviews (claude): 2/3 raised a finding', text)
        self.assertIn('same drafts before seeding: 0/3 reviews raised a finding', text)
        self.assertIn('same drafts before seeding: 1/3 reviews raised a finding', text)

    def test_a_reviewer_that_can_read_files_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            (out / 'tasks' / 'HumanEval-0').mkdir(parents=True)
            (out / 'tasks' / 'HumanEval-0' / 'task.json').write_text(
                json.dumps(study.task_spec(TASK, study.HERE / 'examples' / 'v3-codex.json')))
            run = out / 'runs' / 'HumanEval-0'
            (run / 'round-1' / 'candidate').mkdir(parents=True)
            (run / 'round-1' / 'candidate' / 'solution.py').write_text(GOOD)
            (run / 'round-1' / 'proposal.json').write_text(json.dumps({'rationale': 'r'}))
            (out / 'seeded' / 'HumanEval-0').mkdir(parents=True)
            (out / 'seeded' / 'HumanEval-0' / 'solution.py').write_text(GOOD)
            with self.assertRaisesRegex(ValueError, 'tool-free reviewer'):
                study.review_one(out, 'HumanEval-0', reviewer='codex')


class ProbeTest(unittest.TestCase):
    def test_probe_inputs_are_repeatable_mutations_of_public_inputs_only(self):
        inputs = study.probe_inputs(TASK['base_input'])
        self.assertEqual(inputs, study.probe_inputs(TASK['base_input']))
        self.assertEqual(len(inputs), study.PROBE_INPUTS)
        self.assertEqual(len({repr(i) for i in inputs}), len(inputs))
        self.assertTrue(all(len(i) == 1 and isinstance(i[0], list) for i in inputs))

    def test_a_check_missed_defect_is_told_apart_and_a_rewrite_is_not(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'good.py').write_text(GOOD)
            (root / 'bad.py').write_text(GOOD.replace('max(evens)', 'min(evens)'))
            (root / 'same.py').write_text(TASK['prompt'] + '    return max((x for x in xs if x % 2 == 0), default=None)\n')
            found = study.probe(root, TASK, root / 'good.py', root / 'bad.py')
            alike = study.probe(root, TASK, root / 'good.py', root / 'same.py')
        self.assertGreater(found['differ'], 0)
        self.assertEqual(alike, {'inputs': study.PROBE_INPUTS, 'differ': 0})

    def test_the_report_scores_a_cross_family_draft_as_a_flag_for_hidden_failures(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            cases = [(False, 2, False), (False, 0, True), (True, 1, True), (True, 0, True), (True, 0, False)]
            for i, (r1, differ, other) in enumerate(cases):
                run = out / 'runs' / f'HumanEval-{i}'
                run.mkdir(parents=True)
                (run / 'grade.json').write_text(json.dumps({'round1_pass': r1, 'delivered_pass': r1}))
                (run / 'probe.json').write_text(json.dumps(
                    {'cross-claude': {'inputs': 200, 'differ': differ, 'other_pass': other}}))
            with unittest.mock.patch('sys.stdout', new_callable=__import__('io').StringIO) as printed:
                study.report_all(out, .05)
        self.assertIn('probe, claude draft against round 1: they disagree on local inputs in 2/5 tasks; flagged 1/2 '
                      'where round 1 fails the hidden tests and 1/3 where it passes; either draft passes the hidden '
                      'tests on 4/5, round 1 alone on 3/5', printed.getvalue())

    def test_exceptions_are_outcomes_and_a_hang_is_inconclusive(self):
        crash = TASK['prompt'] + '    raise ValueError\n'
        self.assertEqual(study.compare_here('largest', 0, crash, crash, [[[1]]]), [])
        self.assertEqual(study.compare_here('largest', 0, GOOD, crash, [[[2]]]), [0])
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'good.py').write_text(GOOD)
            (root / 'hang.py').write_text(TASK['prompt'] + '    while True:\n        pass\n')
            with unittest.mock.patch.object(study, 'PROBE_TIMEOUT_S', 2):
                self.assertEqual(study.probe(root, TASK, root / 'good.py', root / 'hang.py'), {'inconclusive': 'timeout'})


if __name__ == '__main__':
    unittest.main()
