"""Adversarial guardrail cases with synthetic data, no real campaigns or model calls."""
import copy
import tempfile
from pathlib import Path
import unittest
from unittest import mock

import evaluate
import releases
from tests import test_evaluate as fixtures


class OverfittingGuardrails(unittest.TestCase):
    def setUp(self):
        patch = mock.patch.object(evaluate, 'identity', fixtures.fake_identity)
        patch.start()
        self.addCleanup(patch.stop)

    def test_new_campaign_names_and_resplit_do_not_reset_validation_allowance(self):
        store = fixtures.Store(groups=10, fractions=(0, 1, 0),
                               policy={'max_validation_submissions': 2})
        for index in range(2):
            evaluate.freeze(store.dir, store.spec(split='validation', campaign=f'campaign-{index}', runs=index + 1))
        evaluate.split(store.dir, seed=987, fractions=(0, 1, 0), resplit=True)
        with self.assertRaisesRegex(evaluate.EvalError, 'used its 2 validation submissions'):
            evaluate.freeze(store.dir, store.spec(split='validation', campaign='new-name', runs=3))

    def test_convenient_aggregate_numbers_cannot_override_any_missing_or_failed_gate(self):
        # Pure in-memory fixtures; these are not stored grades or evidence.
        bounds = {'U': {'t_lower': 1., 'boot_lower': 1.},
                  'W': {'t_upper': 0., 'boot_upper': 0.},
                  'R': {'t_lower': .1, 'boot_lower': .1}}
        spec = {'min_groups': 10, 'budget_usd': 10, 'margins': {'U': .5, 'W': .25},
                'candidate': 'cand', 'comparators': ['base'], 'objective': 'quality', 'max_cost_ratio': 2.}
        report = {'groups': 10, 'failed': [], 'expected_slots': 20, 'completed_slots': 20,
                  'cost_unknown': False, 'integrity_errors': [],
                  'cost_per_review': {'cand': 1., 'base': 1.},
                  'graders': {g: {'missing_grades': [], 'vs': {'base': copy.deepcopy(bounds)}}
                              for g in ('judge-a', 'judge-b')}}
        self.assertEqual(evaluate.gates(spec, report), [])
        for scenario in ('wrong-findings', 'recall', 'second-judge', 'integrity',
                         'missing-grade', 'few-groups', 'unknown-cost', 'failed-slot'):
            changed = copy.deepcopy(report)
            if scenario == 'wrong-findings':
                changed['graders']['judge-b']['vs']['base']['W']['boot_upper'] = 1.
            elif scenario == 'recall':
                changed['graders']['judge-b']['vs']['base']['R']['t_lower'] = 0.
            elif scenario == 'second-judge':
                changed['graders']['judge-b']['vs']['base']['U']['boot_lower'] = 0.
            elif scenario == 'integrity':
                changed['integrity_errors'] = ['changed artifact']
            elif scenario == 'missing-grade':
                changed['graders']['judge-b']['missing_grades'] = ['case']
            elif scenario == 'few-groups':
                changed['groups'] = 9
            elif scenario == 'unknown-cost':
                changed['cost_unknown'] = True
            else:
                changed['failed'] = ['case/cand.1']
                changed['completed_slots'] = 19
            with self.subTest(scenario=scenario):
                self.assertTrue(evaluate.gates(spec, changed))

    def test_noncomparative_test_pass_cannot_be_presented_as_rrsi_improvement(self):
        with tempfile.TemporaryDirectory() as folder:
            spec = {'split': 'test', 'candidate': 'cand', 'comparators': [], 'objective': 'quality',
                    'arms': {'cand': {'plan': None, 'backend': 'claude'}}}
            decision = {'kind': 'decision', 'digest': 'fixture', 'result': 'pass'}
            with mock.patch.object(evaluate, 'checked', return_value=spec), \
                    mock.patch.object(evaluate, 'ledger', return_value=[decision]):
                with self.assertRaisesRegex(ValueError, 'comparative sealed-test evidence'):
                    releases.quality(Path(folder), 'fixture', releases.default_plan(releases.HERE))


if __name__ == '__main__':
    unittest.main()
