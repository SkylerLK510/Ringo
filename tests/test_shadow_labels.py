"""Shadow labels read finished work runs: eligibility, byte identity, tokens by round, regimes and exact bounds."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import shadow_labels
import workflow

GOOD = '@@@ FILE value.py\ndef f(x):\n    return 2*x\n'
OTHER = '@@@ FILE value.py\ndef f(x):\n    return x + x\n'
FINDING = 'What this change does: adds f.\n\n### 1. Style\n- Problem: x.\n'
CLEAN = 'What this change does: adds f.\n\nNo defects found.'


class ShadowLabelTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.spec = {'version': 1, 'brief': 'Implement f(x).\n\nReturn twice x.\n', 'files': {'value.py': ''},
                     'fixtures': {'check.py': "from value import f\nassert f(5) == 10\n"},
                     'checks': [{'name': 'reference', 'argv': [sys.executable, '-B', 'check.py']}],
                     'writer': 'fake', 'judge': 'fake', 'allow_tool_backends': True, 'allow_execution': True,
                     'max_rounds': 3, 'max_seconds': 60, 'protocol': 'text'}
        self.n = 0

    def work(self, replies, review, **changes):
        self.n += 1
        path = self.root / f'task{self.n}.json'
        path.write_text(json.dumps({**self.spec, **changes}))
        writer = iter(replies)

        def call(backend, system, message, timeout_s):
            if system.startswith('Implement the brief'):
                return next(writer), {'input': 100, 'output': 10}
            return review, {'input': 50, 'output': 5}
        out = self.root / f'run{self.n}'
        workflow.run(path, out, call_fn=call)
        return out

    def test_a_revision_that_changes_the_files_is_a_change_and_its_tokens_count_after_round_one(self):
        row = shadow_labels.row(self.work([GOOD, OTHER], FINDING))
        self.assertEqual((row['eligible'], row['identical']), (True, False))
        self.assertEqual((row['round1_tokens'], row['after_tokens']), (110, 165))  # review 55 + revision 110

    def test_a_clean_review_delivers_round_one_unchanged(self):
        row = shadow_labels.row(self.work([GOOD], CLEAN))
        self.assertTrue(row['identical'])

    def test_grades_give_harm_and_avoided_regressions(self):
        harmed, avoided = self.work([GOOD, OTHER], FINDING), self.work([GOOD, OTHER], FINDING)
        (harmed / 'grade.json').write_text(json.dumps({'round1_pass': False, 'delivered_pass': True}))
        (avoided / 'grade.json').write_text(json.dumps({'round1_pass': True, 'delivered_pass': False}))
        rows = [shadow_labels.row(r) for r in (harmed, avoided)]
        self.assertEqual([(r['harm'], r['avoided']) for r in rows], [(True, False), (False, True)])
        self.assertIn('graded harm: 1/2', shadow_labels.summary(rows, .05))

    def test_tasks_share_a_regime_and_a_settings_change_starts_a_new_one(self):
        a = shadow_labels.row(self.work([GOOD], CLEAN))
        b = shadow_labels.row(self.work([GOOD], CLEAN, brief='Implement f(x).\n\nDouble x.\n'))
        c = shadow_labels.row(self.work([GOOD], CLEAN, max_rounds=2))
        self.assertEqual(a['regime'], b['regime'])
        self.assertNotEqual(a['regime'], c['regime'])

    def test_upper_bound_is_exact_clopper_pearson(self):
        self.assertAlmostEqual(shadow_labels.upper_bound(0, 149), 1 - .05 ** (1 / 149), places=6)
        self.assertLess(shadow_labels.upper_bound(0, 149), .02)
        self.assertGreater(shadow_labels.upper_bound(0, 148), .02)
        self.assertAlmostEqual(shadow_labels.upper_bound(1, 10), .3942, places=4)
        self.assertEqual(shadow_labels.upper_bound(3, 3), 1.0)

    def test_folders_are_found_below_a_parent(self):
        self.work([GOOD], CLEAN)
        self.work([GOOD], CLEAN)
        self.assertEqual(len(list(shadow_labels.runs([self.root]))), 2)


if __name__ == '__main__':
    unittest.main()
