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
    def test_a_task_is_a_v3_work_spec_in_shadow_mode(self):
        spec = study.task_spec(TASK, study.HERE / 'examples' / 'v3-codex.json')
        self.assertEqual(spec['stop_on_checks'], {'retain': 1})
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

    def test_seeding_stops_at_the_limit_when_results_already_exist(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            for i in range(3):
                run = out / 'runs' / f'HumanEval-{i}'
                run.mkdir(parents=True)
                (run / 'grade.json').write_text(json.dumps({'round1_pass': True, 'delivered_pass': True}))
                (run / 'report.json').write_text(json.dumps({'stop': {'eligible': True}, 'rounds': []}))
                (out / 'seeded' / run.name).mkdir(parents=True)
                (out / 'seeded' / run.name / 'result.json').write_text('{}')
            with unittest.mock.patch.object(study, 'dataset', return_value={}), \
                    unittest.mock.patch.object(study.subprocess, 'run') as launched:
                study.seed_all(out, 2, 1)
            launched.assert_not_called()  # two results exist: nothing new is reviewed


if __name__ == '__main__':
    unittest.main()
