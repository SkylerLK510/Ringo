"""Benchmark contract tests use fixture answers; none are model performance results."""
import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import harbenchmark as hb


def fixture(ids=None):
    manifest = hb.export(ids)
    sub = copy.deepcopy(manifest['submission_template'])
    sub['metadata'] = {'harness': 'test-fixture', 'model': 'none', 'revision': 'test',
                       'run_kind': 'fixture', 'evidence': 'tests/test_harbenchmark.py'}
    sub['answers'] = {c['id']: {'status': 'ok', 'answer': copy.deepcopy(c['expected'])}
                      for c in hb.selected(sub['case_ids'])}
    return manifest, sub


class HarBenchmarkTest(unittest.TestCase):
    def test_reference_fixtures_cover_six_tracks_without_claiming_model_results(self):
        manifest, sub = fixture()
        report = hb.score(sub, manifest)
        self.assertEqual(len(report['cases']), 12)
        self.assertEqual(len(report['tracks']), 6)
        self.assertTrue(report['coverage']['complete'])
        self.assertEqual(report['metadata']['run_kind'], 'fixture')
        for row in report['tracks'].values():
            self.assertEqual((row['selected'], row['passed'], row['pass_rate']), (2, 2, 1))
        self.assertNotIn('overall_score', report)

    def test_missing_failed_unsupported_and_malformed_keep_schedule_denominator(self):
        manifest, sub = fixture()
        sub['answers'].pop('review-clean')
        sub['answers']['review-fallback'] = {'status': 'failed'}
        sub['answers']['repair-zero'] = {'status': 'unsupported'}
        sub['answers']['repair-none'] = {'status': 'ok'}
        report = hb.score(sub, manifest)
        self.assertEqual(report['tracks']['review']['selected'], 2)
        self.assertEqual(report['tracks']['review']['pass_rate'], 0)
        self.assertEqual(report['tracks']['coding']['unsupported'], 1)
        self.assertEqual(report['tracks']['coding']['malformed'], 1)
        self.assertEqual(len(report['cases']), 12)

    def test_incorrect_values_extra_fields_and_bool_cannot_pass(self):
        for answer in ({'defects': True}, {'defects': 0.0}, {'defects': 1}, {'defects': 0, 'extra': 'stuff'},
                       'no defects found', None):
            manifest, sub = fixture(['review-clean'])
            sub['answers']['review-clean']['answer'] = answer
            with self.subTest(answer=answer):
                self.assertEqual(hb.score(sub, manifest)['tracks']['review']['passed'], 0)

    def test_ast_repairs_accept_formatting_but_reject_different_behavior(self):
        manifest, sub = fixture(['repair-zero'])
        for expr, passed in [("data.get( 'balance',default )", 1),
                             ('data.get("balance") or default', 0),
                             ('__import__("os").system("false")', 0), ('not valid python!', 0)]:
            sub['answers']['repair-zero']['answer']['replacement'] = expr
            with self.subTest(expr=expr):
                self.assertEqual(hb.score(sub, manifest)['tracks']['coding']['passed'], passed)

    def test_strict_json_rejects_duplicates_and_nonfinite_numbers_recursively(self):
        for text in ('{"answers":{},"answers":{}}', '{"x":NaN}', '{"x":[1e999]}', '{"x":-Infinity}'):
            with self.subTest(text=text), self.assertRaises(ValueError):
                hb.strict_json(text)

    def test_suite_identity_and_original_schedule_are_required(self):
        manifest, sub = fixture()
        for mutation in ('hash', 'selected', 'unknown_answer', 'extra_top_key', 'duplicate', 'prompt'):
            m, s = copy.deepcopy(manifest), copy.deepcopy(sub)
            if mutation == 'hash':
                s['suite_sha256'] = '0' * 64
            elif mutation == 'selected':
                s['case_ids'] = s['case_ids'][:-1]
            elif mutation == 'unknown_answer':
                s['answers']['invented'] = {'status': 'ok', 'answer': 0}
            elif mutation == 'extra_top_key':
                s['overall_score'] = 1
            elif mutation == 'duplicate':
                m['cases'].append(m['cases'][0])
            else:
                m['cases'][0]['task'] = 'Changed after generation'
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                hb.score(s, m)

    def test_numeric_tolerance_and_nested_totals(self):
        self.assertTrue(hb.equal(1.0000005, 1))
        self.assertFalse(hb.equal(1.000002, 1))
        manifest, sub = fixture(['artifact-table'])
        sub['answers']['artifact-table']['answer']['rows'][-1]['pass_rate'] = 0.7
        self.assertEqual(hb.score(sub, manifest)['tracks']['artifact']['passed'], 0)

    def test_large_valid_integer_fails_one_case_without_discarding_other_results(self):
        manifest, sub = fixture()
        sub['answers']['design-capacity']['answer']['hourly_usd'] = 10 ** 400
        sub = hb.strict_json(json.dumps(sub))
        report = hb.score(sub, manifest)
        self.assertEqual(len(report['cases']), 12)
        self.assertEqual(report['tracks']['design']['passed'], 1)
        self.assertEqual(report['tracks']['design']['failed'], 1)

    def test_uncertainty_answer_at_required_seven_significant_digits_passes(self):
        manifest, sub = fixture(['research-uncertainty'])
        sub['answers']['research-uncertainty']['answer'] = {
            'mean': 5, 'sample_variance': 6.666667, 'standard_error': 1.290994}
        self.assertIn('seven significant digits', manifest['cases'][0]['task'])
        self.assertEqual(hb.score(sub, manifest)['tracks']['research']['passed'], 1)

    def test_cli_round_trip_no_overwrite_and_failure_exit(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            manifest, sub = fixture(['review-clean'])
            (root / 'manifest.json').write_text(json.dumps(manifest))
            (root / 'submission.json').write_text(json.dumps(sub))
            argv = ['score', '--manifest', str(root / 'manifest.json'), '--submission',
                    str(root / 'submission.json'), '--out', str(root / 'report.json')]
            self.assertEqual(hb.main(argv), 0)
            self.assertEqual(hb.main(argv), 2)
            sub['answers'] = {}
            (root / 'submission.json').write_text(json.dumps(sub))
            argv[-1] = str(root / 'missing.json')
            self.assertEqual(hb.main(argv), 1)
            self.assertEqual(json.loads((root / 'missing.json').read_text())['cases'][0]['status'], 'missing')

    def test_exports_do_not_include_reference_answers_and_hash_covers_source(self):
        self.assertNotIn('expected', json.dumps(hb.export()))
        original = hb.suite_hash()
        with mock.patch.object(Path, 'read_bytes', return_value=b'changed scoring source'):
            self.assertNotEqual(original, hb.suite_hash())


if __name__ == '__main__':
    unittest.main()
