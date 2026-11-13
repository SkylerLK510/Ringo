"""Behavioral tests of recursive advancement, sealed access and honest restart/budget accounting.
All evaluators and proposers here are deterministic fakes; no inference occurs.
"""
import copy
import json
from pathlib import Path
import tempfile
import unittest

import turnrun_campaign as rrsi


def plan():
    tasks = {'dev': ['D1', 'D2', 'D3'], 'validation': ['V1'], 'holdout': ['H1']}
    return {'version': 1, 'splits': tasks, 'clusters': {t: t for ts in tasks.values() for t in ts},
            'coverage': ['instruction', 'handoff_instruction', 'handoff_at'],
            'baseline': {}, 'degraded': {'instruction': 'degraded'}, 'manual': {'instruction': 'manual'},
            'budgets': {p: 100000 for p in rrsi.PHASES}, 'run_cap': 1000, 'proposal_cap': 100,
            'margin': 1000, 'seconds': 30, 'rounds': 2, 'repetitions': 3, 'search_repetitions': 2,
            'seed': 12, 'min_baseline': 0.5, 'min_sensitivity': 0.2,
            'min_gain': 0.1, 'min_savings': 0.1, 'quality_tolerance': 0, 'cost_allowance': 0.1}


def record(tokens=100, **data):
    return {'outcome': 'done', 'usage': {'input': tokens//2, 'cache_write': 0, 'cache_read': tokens-tokens//2,
            'output': 0, 'counted': True, 'uncounted_calls': 0}, **data}


class CampaignTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'campaign'
        self.calls = []
        self.proposals = []
        self.campaign = rrsi.Campaign.create(self.root, plan())

    def evaluate(self, policy, task, rep, cap, seconds):
        self.calls.append((copy.deepcopy(policy), task, rep))
        costs = {'': 100, 'degraded': 100, 'first': 70, 'second': 50, 'manual': 80, 'worse': 200}
        return record(costs.get(policy['instruction'], 100), passed=policy['instruction'] != 'degraded',
                      protected_pass=True)

    def propose(self, parent, history, round_no, sibling, cap):
        self.proposals.append((copy.deepcopy(parent), copy.deepcopy(history), round_no, sibling))
        text = ('first' if round_no == 1 else 'second') if sibling == 1 else 'worse'
        return record(10, policy={**parent, 'instruction': text}, hypothesis=f'round {round_no} sibling {sibling}')

    def test_two_siblings_advance_recursively_and_confirmation_stays_sealed(self):
        control = self.campaign.controls(self.evaluate)
        self.assertTrue(control['passed'])
        final = self.campaign.search(self.evaluate, self.propose)
        self.assertEqual(final['incumbent']['instruction'], 'second')
        self.assertEqual(len(self.proposals), 4)
        self.assertEqual(self.proposals[0][0], self.proposals[1][0])
        self.assertEqual(self.proposals[2][0]['instruction'], 'first')
        self.assertEqual(self.proposals[2][0], self.proposals[3][0])
        self.assertEqual(self.proposals[0][1], self.proposals[1][1])
        self.assertTrue(any(e['kind'] == 'decision' for e in self.proposals[2][1]))
        self.assertFalse(any(t.startswith(('V', 'H')) for _, t, _ in self.calls))
        complete = self.campaign.confirm(self.evaluate)
        self.assertTrue(complete['passed'])
        self.assertFalse(complete['adoption'])
        self.assertFalse(complete['certification'])
        self.assertLess(complete['search_vs_manual']['token_delta'], 0)
        self.assertTrue(complete['search_vs_manual']['equal_admission_ceilings'])
        self.assertTrue(any(t == 'H1' for _, t, _ in self.calls))
        history = self.campaign.dev_history()
        self.assertFalse(any(e.get('metadata', {}).get('split') in ('validation', 'holdout') for e in history))
        calls = len(self.calls)
        self.campaign.controls(self.evaluate)
        self.campaign.search(self.evaluate, self.propose)
        self.campaign.confirm(self.evaluate)
        self.assertEqual(len(self.calls), calls, 'completed operations are idempotent')

    def test_failed_controls_block_search(self):
        def insensitive(*args):
            return record(100, passed=True, protected_pass=True)
        self.assertFalse(self.campaign.controls(insensitive)['passed'])
        with self.assertRaisesRegex(ValueError, 'sensitivity'):
            self.campaign.search(self.evaluate, self.propose)
        self.assertEqual(self.proposals, [])

    def test_uncovered_or_multiple_changes_are_screened_after_charging_proposals(self):
        self.campaign.controls(self.evaluate)
        def bad(parent, *args):
            return record(10, policy={**parent, 'instruction': 'new', 'handoff_at': 32000}, hypothesis='two edits')
        self.campaign.search(self.evaluate, bad)
        rejected = [e for e in self.campaign.verify() if e['kind'] == 'proposal']
        self.assertEqual(len(rejected), 4)
        self.assertTrue(all(e['policy'] is None for e in rejected))
        self.assertEqual(self.campaign.find('search_complete')['incumbent'], rrsi.policy({}))
        self.assertGreaterEqual(self.campaign.totals()['search']['known'], 40)

    def test_unknown_counts_reserve_cap_and_halt(self):
        def unknown(*args):
            return {'outcome': 'done', 'passed': True, 'protected_pass': True}
        with self.assertRaisesRegex(ValueError, 'usage/delivery'):
            self.campaign.controls(unknown)
        self.assertEqual(self.campaign.totals()['controls']['reserved'], 1000)
        with self.assertRaisesRegex(ValueError, 'halted'):
            rrsi.Campaign(self.root).controls(self.evaluate)

    def test_interrupted_attempt_never_retries_and_keeps_reservation(self):
        def interrupted(*args):
            raise KeyboardInterrupt()
        with self.assertRaises(KeyboardInterrupt):
            self.campaign.controls(interrupted)
        self.assertEqual(self.campaign.totals()['controls']['reserved'], 1000)
        with self.assertRaisesRegex(ValueError, 'interrupted attempt'):
            rrsi.Campaign(self.root).controls(self.evaluate)
        self.assertFalse(self.calls)

    def test_overshoot_charged_and_stops_later_inference(self):
        with self.assertRaisesRegex(ValueError, 'usage/delivery'):
            self.campaign.controls(lambda *a: record(1001, passed=True, protected_pass=True))
        self.assertEqual(self.campaign.totals()['controls']['known'], 1001)
        self.assertIsNotNone(self.campaign.find('halt'))

    def test_stop_margin_blocks_admission_before_call(self):
        other = plan()
        other['budgets']['controls'] = 1999
        c = rrsi.Campaign.create(Path(self.temp.name) / 'small', other)
        with self.assertRaisesRegex(ValueError, 'admission'):
            c.controls(self.evaluate)
        self.assertEqual(self.calls, [])

    def test_validation_failure_never_opens_holdout_or_search_again(self):
        self.campaign.controls(self.evaluate)
        self.campaign.search(self.evaluate, self.propose)
        def evaluator(p, t, *args):
            result = self.evaluate(p, t, *args)
            if t == 'V1' and p['instruction'] == 'second':
                result['passed'] = False
            return result
        result = self.campaign.confirm(evaluator)
        self.assertEqual(result['stopped_at'], 'validation')
        self.assertFalse(any(t == 'H1' for _, t, _ in self.calls))
        self.campaign.search(self.evaluate, self.propose)  # idempotent: no new search calls
        self.assertEqual(len(self.proposals), 4)

    def test_protected_regression_halts(self):
        with self.assertRaisesRegex(ValueError, 'protected regression'):
            self.campaign.controls(lambda *a: record(passed=True, protected_pass=False))
        self.assertEqual(self.campaign.totals()['controls']['known'], 100)

    def test_changed_manifest_and_tampered_ledger_refused(self):
        path = self.root / 'plan.json'
        body = json.loads(path.read_text())
        body['seed'] += 1
        path.write_text(json.dumps(body))
        with self.assertRaisesRegex(ValueError, 'manifest changed'):
            rrsi.Campaign(self.root)
        path.write_text(json.dumps(self.campaign.plan))
        self.campaign.append('dummy')
        lines = self.campaign.path.read_text().splitlines()
        self.campaign.path.write_text(lines[1] + '\n' + lines[0] + '\n')
        with self.assertRaisesRegex(ValueError, 'chain'):
            rrsi.Campaign(self.root)

    def test_splits_and_security_fields_fail_closed(self):
        for key in ('tools', 'network', 'permissions', 'model', 'auth'):
            with self.subTest(key=key), self.assertRaises(ValueError):
                rrsi.policy({key: 'anything'})
        p = plan()
        p['clusters']['V1'] = 'D1'
        with self.assertRaisesRegex(ValueError, 'crosses splits'):
            rrsi.validate_plan(p)
        p = plan()
        p['splits']['holdout'] = ['V1']
        with self.assertRaises(ValueError):
            rrsi.validate_plan(p)

    def test_sealed_task_cannot_be_mislabelled_before_or_after_controls(self):
        for stage in (False, True):
            if stage:
                self.campaign.controls(self.evaluate)
            count = len(self.calls)
            for task, split, phase in [('H1', 'dev', 'search'), ('H1', 'holdout', 'confirmation'),
                                       ('V1', 'validation', 'controls'), ('V1', 'dev', 'controls')]:
                with self.subTest(task=task, phase=phase), self.assertRaises(ValueError):
                    self.campaign.evaluation(self.evaluate, rrsi.policy({}), task, 1, phase, 'forged', split)
            self.assertEqual(len(self.calls), count)

    def test_crash_after_finish_does_not_launder_unknown_usage_or_overshoot(self):
        for suffix, result in [('unknown', {'outcome': 'done'}), ('over', record(1001))]:
            c = rrsi.Campaign.create(Path(self.temp.name) / suffix, plan())
            original = c.append
            def append(kind, **data):
                saved = original(kind, **data)
                if kind == 'finish':
                    raise KeyboardInterrupt()
                return saved
            c.append = append
            with self.assertRaises(KeyboardInterrupt):
                c.call('crash', 'controls', 1000, lambda: result)
            resumed = rrsi.Campaign(c.root)
            with self.assertRaisesRegex(ValueError, 'usage/delivery'):
                resumed.call('crash', 'controls', 1000, lambda: self.fail('must not retry'))

    def test_cached_request_metadata_cannot_change(self):
        self.campaign.call('cached', 'controls', 1000, lambda: record(), task='D1')
        for phase, cap, metadata in [('search', 1000, {'task': 'D1'}),
                                     ('controls', 2000, {'task': 'D1'}),
                                     ('controls', 1000, {'task': 'H1'})]:
            with self.assertRaisesRegex(ValueError, 'pinned request'):
                self.campaign.call('cached', phase, cap, lambda: self.fail('must not call'), **metadata)

    def test_unicode_boundary_matches_rust_policy_schema(self):
        for text in ('\x00', '\x1b', '\x7f', '\u0085', '\u009f', 'a'*4097):
            with self.subTest(text=repr(text)), self.assertRaises(ValueError):
                rrsi.policy({'instruction': text})
        self.assertEqual(rrsi.policy({'instruction': 'line\n\t世界'})['instruction'], 'line\n\t世界')


if __name__ == '__main__':
    unittest.main()
