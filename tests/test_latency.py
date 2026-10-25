"""Synthetic policy tests, not observed model or machine performance claims."""
import copy
import json
from pathlib import Path
import tempfile
import unittest

import latency as lt

POLICY = {'absolute_ms': 5, 'relative': .05, 'min_pairs': 100, 'resamples': 500, 'seed': 7}


def data(candidate=1.0, baseline=1.0, count=100):
    return {'version': 1, 'baseline': 'baseline-id', 'candidate': 'candidate-id', 'planned_pairs': count,
            'protocol': {'workload': 'review', 'environment': 'synthetic-unit-test',
                         'baseline_config': 'plan-a', 'candidate_config': 'plan-b', 'order': 'alternating',
                         'warmups': 3, 'clock': 'perf_counter_ns', 'kind': 'simulated'},
            'pairs': [{'id': str(i), 'baseline_s': baseline, 'candidate_s': candidate,
                       'baseline_ok': True, 'candidate_ok': True} for i in range(count)]}


class LatencyTest(unittest.TestCase):
    def test_equal_pairs_fit_budgets_and_have_zero_bounds(self):
        report = lt.assess(data(), POLICY)
        self.assertEqual(report['status'], 'negligible')
        self.assertEqual(report['metrics']['mean_delta_ms']['interval'], [0, 0])

    def test_clear_absolute_slowdown(self):
        self.assertEqual(lt.assess(data(1.02), POLICY)['status'], 'slower')

    def test_relative_budget_independently_blocks_fast_short_workloads(self):
        report = lt.assess(data(.0012, .001), POLICY)
        self.assertLess(report['metrics']['mean_delta_ms']['estimate'], 5)
        self.assertEqual(report['status'], 'slower')

    def test_absolute_budget_independently_blocks_long_workloads(self):
        self.assertEqual(lt.assess(data(10.01, 10), POLICY)['status'], 'slower')

    def test_speedup_is_acceptable_without_inventing_a_quality_gain(self):
        report = lt.assess(data(.8), POLICY)
        self.assertEqual(report['status'], 'negligible')
        self.assertLess(report['metrics']['mean_delta_ms']['interval'][1], 0)

    def test_small_sample_is_uncertain_even_if_identical(self):
        self.assertEqual(lt.assess(data(count=20), POLICY)['status'], 'uncertain')

    def test_wide_uncertainty_is_not_called_negligible(self):
        d = data()
        for i, pair in enumerate(d['pairs']):
            pair['baseline_s'] = .9 + i * .002
            pair['candidate_s'] = pair['baseline_s'] + .005 + (.004 if i % 2 else -.004)
        self.assertEqual(lt.assess(d, POLICY)['status'], 'uncertain')

    def test_tail_regression_is_detected_even_when_mean_fits(self):
        d = data(count=1000)
        for pair in d['pairs'][-100:]:
            pair['candidate_s'] += .04
        report = lt.assess(d, POLICY)
        self.assertLess(report['metrics']['mean_delta_ms']['estimate'], 5)
        self.assertEqual(report['status'], 'slower')
        self.assertGreater(report['metrics']['p95_delta_ms']['interval'][0], 5)

    def test_new_fast_failure_blocks_activation(self):
        d = data(.01)
        d['pairs'][0]['candidate_ok'] = False
        self.assertEqual(lt.assess(d, POLICY)['status'], 'slower')

    def test_equal_failures_are_uncertain_not_success(self):
        d = data()
        d['pairs'][0]['candidate_ok'] = d['pairs'][0]['baseline_ok'] = False
        self.assertEqual(lt.assess(d, POLICY)['status'], 'uncertain')

    def test_failures_and_small_samples_cannot_mask_a_demonstrated_slowdown(self):
        for count in (10,100):
            d = data(2.0,count=count)
            d['pairs'][0]['baseline_ok'] = d['pairs'][0]['candidate_ok'] = False
            with self.subTest(count=count):
                self.assertEqual(lt.assess(d,POLICY)['status'],'slower')

    def test_missing_pair_is_invalid_not_dropped(self):
        d = data()
        d['pairs'].pop()
        with self.assertRaises(ValueError):
            lt.assess(d, POLICY)

    def test_duplicate_pair_is_invalid(self):
        d = data()
        d['pairs'][1]['id'] = '0'
        with self.assertRaises(ValueError):
            lt.assess(d, POLICY)

    def test_duration_edges_reject_bool_nonfinite_negative_zero_and_huge(self):
        for value in (True, float('nan'), float('inf'), -1, 0, 10**400, 86401):
            d = data()
            d['pairs'][0]['baseline_s'] = value
            with self.subTest(value=str(value)[:20]), self.assertRaises(ValueError):
                lt.assess(d, POLICY)

    def test_protocol_and_counts_must_be_explicit(self):
        for key in ('workload', 'environment', 'candidate_config', 'order', 'warmups', 'clock', 'kind'):
            d = data()
            d['protocol'].pop(key)
            with self.subTest(key=key), self.assertRaises(ValueError):
                lt.assess(d, POLICY)
        for key, value in [('planned_pairs', True), ('planned_pairs', 0), ('candidate', '')]:
            d = data()
            d[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                lt.assess(d, POLICY)

    def test_policy_rejects_invalid_budgets_and_work_bounds(self):
        for key, value in [('relative', -1), ('absolute_ms', float('inf')), ('min_pairs', 1),
                           ('resamples', 1), ('resamples', 50001), ('seed', True)]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                lt.assess(data(), {**POLICY, key: value})

    def test_tampered_verdict_data_policy_and_assessor_are_rejected(self):
        report = lt.assess(data(), POLICY)
        self.assertEqual(lt.verify_report(report), report)
        for change in ('status', 'measurement', 'policy', 'assessor_sha256'):
            r = copy.deepcopy(report)
            if change == 'measurement':
                r[change]['pairs'][0]['candidate_s'] = 1.5
            elif change == 'policy':
                r[change]['absolute_ms'] = 10
            else:
                r[change] = 'forged'
            with self.subTest(change=change), self.assertRaises(ValueError):
                lt.verify_report(r)

    def test_json_roundtrip_preserves_recomputable_report(self):
        report = lt.assess(data(1.001), POLICY)
        self.assertEqual(lt.verify_report(json.loads(json.dumps(report))), report)

    def test_report_boolean_is_not_equal_to_a_numeric_integrity_field(self):
        report = lt.assess(data(),POLICY)
        report['version'] = True
        with self.assertRaises(ValueError):
            lt.verify_report(report)

    def test_json_duplicate_and_nonfinite_constants_are_rejected(self):
        with tempfile.TemporaryDirectory() as folder:
            p = Path(folder) / 'input.json'
            for text in ('{"id":1,"id":2}', '{"value":NaN}'):
                p.write_text(text)
                with self.assertRaises(ValueError):
                    lt.read_json(p)


if __name__ == '__main__':
    unittest.main()
