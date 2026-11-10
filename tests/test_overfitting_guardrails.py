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

    def test_a_token_limit_refuses_more_tokens_and_unknown_token_use(self):
        # The cost gate is in dollars. A backend that reports no price is charged the declared price per call, so
        # two plans that make the same calls cost the same whatever their tokens: only a token limit sees the rest.
        bounds = {'U': {'t_lower': 1., 'boot_lower': 1.},
                  'W': {'t_upper': 0., 'boot_upper': 0.},
                  'R': {'t_lower': .1, 'boot_lower': .1}}
        spec = {'min_groups': 10, 'budget_usd': 10, 'margins': {'U': .5, 'W': .25}, 'max_token_ratio': 1.0,
                'candidate': 'cand', 'comparators': ['base'], 'objective': 'quality', 'max_cost_ratio': 2.}
        report = {'groups': 10, 'failed': [], 'expected_slots': 20, 'completed_slots': 20,
                  'cost_unknown': False, 'integrity_errors': [],
                  'cost_per_review': {'cand': .05, 'base': .05}, 'tokens_per_review': {'cand': 30500., 'base': 10500.},
                  'graders': {g: {'missing_grades': [], 'vs': {'base': copy.deepcopy(bounds)}}
                              for g in ('judge-a', 'judge-b')}}
        self.assertEqual(evaluate.gates({**spec, 'max_token_ratio': None}, report), [], 'no limit: the dollars decide')
        self.assertEqual(evaluate.gates(spec, report), [('fail', 'tokens vs base: 2.90x (limit 1.00x)')])
        self.assertEqual(evaluate.gates({**spec, 'max_token_ratio': 3.0}, report), [])
        equal = {**report, 'tokens_per_review': {'cand': 10500., 'base': 10500.}}
        self.assertEqual(evaluate.gates(spec, equal), [])
        # unknown is not zero: an arm with a call that reported no tokens, or a report scored before tokens were
        unknown = {**report, 'tokens_per_review': {'cand': None, 'base': 10500.}}
        self.assertEqual(evaluate.gates(spec, unknown), [('inconclusive', 'tokens vs base unknown')])
        older = {k: v for k, v in report.items() if k != 'tokens_per_review'}
        self.assertEqual(evaluate.gates(spec, older), [('inconclusive', 'tokens vs base unknown')])

    def test_a_store_that_limits_tokens_refuses_an_experiment_that_does_not(self):
        store = fixtures.Store(groups=10, fractions=(0, 1, 0), policy={'max_token_ratio': 1.0})
        for loose in ({}, {'max_token_ratio': 1.5}):
            with self.subTest(loose=loose), self.assertRaisesRegex(evaluate.EvalError, 'looser than the store policy'):
                evaluate.freeze(store.dir, store.spec(split='validation', **loose))
        evaluate.freeze(store.dir, store.spec(split='validation', max_token_ratio=1.0))
        for bad in (0, -1, 'x', True, float('inf')):
            with self.subTest(bad=bad), self.assertRaisesRegex(evaluate.EvalError, 'max_token_ratio'):
                evaluate.load_spec(store.spec(split='validation', max_token_ratio=bad))
        with self.assertRaisesRegex(evaluate.EvalError, 'policy'):
            fixtures.Store(groups=10, fractions=(0, 1, 0), policy={'max_token_ratio': 0})

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
