"""Offline checks of improve.py: a fake case pool, fake reviewers, graders and proposer; no model calls."""
import contextlib
import json
import re
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import evaluate as ev  # noqa: E402
import improve as im  # noqa: E402

from tests.test_evaluate import fake_identity

_patch = None

def setUpModule():
    global _patch
    _patch = mock.patch.object(ev, 'identity', fake_identity)
    _patch.start()

def tearDownModule():
    _patch.stop()

GOLD = ('- **G1** (high, correctness, difficulty hard) `src/app.py:1-2`: the parser drops the last record when the '
        'input ends without a trailing newline\n- **G2** (low, reliability, difficulty easy) `src/io.py:3-4`: a '
        'failed write leaves a partial file behind\n')
BASE_PROMPT = ('Review the change in the material and report the real defects it introduces: wrong results, '
               'crashes, security holes, lost data and failures at the expected load. Read beyond the diff: '
               'callers, readers and every promise the docs make. Show each defect with a concrete input, '
               'what happens and what should happen, and quote the lines it rests on. A claim that depends on '
               'code you cannot see is a question, not a finding. No style comments.')
USAGE = {'input': 1, 'cache_write': 10, 'cache_read': 0, 'output': 5, 'cost_usd': 0.01}


class World:
    def __init__(self, proposals):
        self.dir = Path(tempfile.mkdtemp())
        pool = {}
        for k in range(24):
            cid = f'case{k}'
            d = self.dir / 'store' / 'cases' / cid
            d.mkdir(parents=True)
            (d / 'material.txt').write_text(f'=== change.diff ===\n+ line {k} MATERIAL-{cid}\n', encoding='utf-8')
            (d / 'gold.md').write_text(f'CANARY canary{k:028d}\n{GOLD}', encoding='utf-8')
            pool[cid] = {'group': f'owner/repo{k}', 'source': 'test', 'language': 'Python', 'created': '2026-10-01'}
        self.store = self.dir / 'store'
        (self.store / 'cases.json').write_text(json.dumps({'cases': pool}), encoding='utf-8')
        (self.store / 'canaries.json').write_text(json.dumps({f'case{k}': f'canary{k:028d}' for k in range(24)}))
        ev.split(self.store, seed=2, fractions=(0.5, 0.5, 0))
        (self.dir / 'prompts').mkdir()
        (self.dir / 'prompts' / 'review.md').write_text(BASE_PROMPT, encoding='utf-8')
        self.plan = self.dir / 'plan.json'
        self.plan.write_text(json.dumps({'version': 1, 'kind': 'review', 'role': 'r', 'prompts_dir': 'prompts',
                                         'passes': [{'id': 'review', 'prompt': 'review.md', 'reply': 'findings'}]}))
        self.campaign = self.dir / 'campaign.json'
        self.campaign.write_text(json.dumps({'name': 'c1', 'incumbent': 'plan.json', 'backend': 'reviewer',
                                             'proposer': 'proposer', 'graders': ['grader-claude', 'grader-codex'],
                                             'max_proposals': 3, 'dev_sample': 12, 'bootstrap': 300,
                                             'unpriced_call_usd': 0.0, 'allow_tool_backends': True}))
        self.proposals, self.proposer_saw = list(proposals), []

    def call(self, backend, system, message, timeout_s, model=None):
        who = backend['argv'][0]
        if who == 'proposer':
            self.proposer_saw.append(system + message)
            return json.dumps(self.proposals.pop(0)), dict(USAGE)
        if 'grader' in who:
            ids = re.findall(r'=== Review (r-[0-9a-f]+) ===', message)
            bodies = re.split(r'=== Review r-[0-9a-f]+ ===', message)[1:]
            return json.dumps({rid: {'found': sorted(set(re.findall(r'G\d+', b))), 'invalid': [], 'valid_extra': [],
                                     'style': [], 'decoy_flagged': []} for rid, b in zip(ids, bodies)}), None
        found = 'G1 and G2' if 'state the failing input first' in message else 'G1'
        return f'What this change does: x.\n\n### 1. Defects {found} (`a.py:1`)\n- Problem: p', dict(USAGE)


def edit(text):
    return {'hypothesis': 'asking for the failing input first finds more defects',
            'prompts': {'review.md': text}, 'expected': {'calls_per_review': 1, 'token_ratio': 1.05}}


class CampaignTest(unittest.TestCase):
    def test_an_existing_campaign_cannot_reset_its_budget_in_another_workspace(self):
        w = World([edit(BASE_PROMPT + ' Be thorough.')] * 3)
        im.campaign(w.store, w.campaign, w.dir / 'ws', call_fn=w.call)
        camp = json.loads(w.campaign.read_text())
        w.campaign.write_text(json.dumps({**camp, 'seed': 42}))
        calls = []
        with self.assertRaisesRegex(ev.EvalError, 'budget cannot be reset'):
            im.campaign(w.store, w.campaign, w.dir / 'other-ws', call_fn=lambda *a: calls.append(a))
        self.assertEqual(calls, [])

    def test_a_nonempty_workspace_is_refused_before_any_call(self):
        w = World([])
        ws = w.dir / 'ws'
        ws.mkdir()
        (ws / 'log.json').write_text('earlier run')
        with self.assertRaisesRegex(ev.EvalError, 'new or empty'):
            im.campaign(w.store, w.campaign, ws, call_fn=w.call)
        self.assertEqual((ws / 'log.json').read_text(), 'earlier run')
        self.assertFalse(any(r['kind'] == 'prereg' for r in ev.ledger(w.store)))

    def test_a_dev_gain_with_a_missing_grade_never_reaches_validation(self):
        w = World([edit(BASE_PROMPT + ' For each defect, state the failing input first.')] * 3)
        cid = ev.split_cases(w.store, 'dev')[0]

        def missing(backend, system, message, timeout_s, model=None):
            if 'grader' in backend['argv'][0] and 'G2' in message and f'MATERIAL-{cid}\n' in system:
                return '{}', dict(USAGE)
            return w.call(backend, system, message, timeout_s, model)
        log = im.campaign(w.store, w.campaign, w.dir / 'ws', call_fn=missing)
        self.assertTrue(any('incomplete' in str(e.get('why')) for e in log))
        self.assertFalse(any(r['kind'] == 'prereg' and r['split'] == 'validation' for r in ev.ledger(w.store)))

    def test_a_proposer_that_spends_the_rest_of_the_budget_stops_cleanly(self):
        w = World([edit(BASE_PROMPT + ' For each defect, state the failing input first.')])
        camp = json.loads(w.campaign.read_text())
        w.campaign.write_text(json.dumps({**camp, 'budget_usd': 1}))

        def expensive(backend, *args):
            text, usage = w.call(backend, *args)
            return text, {'cost_usd': 2} if backend['argv'][0] == 'proposer' else usage
        log = im.campaign(w.store, w.campaign, w.dir / 'ws', call_fn=expensive)
        self.assertEqual(log[0]['stopped'], 'budget')
        self.assertEqual(sum(r['kind'] == 'prereg' for r in ev.ledger(w.store)), 1)
        self.assertGreater(log[-1]['cost_usd'], 2)

    def test_a_grading_interruption_keeps_the_experiments_spending_in_the_campaign_log(self):
        w = World([])
        with mock.patch.object(ev, 'grade', side_effect=RuntimeError('interrupted')):
            with self.assertRaisesRegex(RuntimeError, 'interrupted'):
                im.campaign(w.store, w.campaign, w.dir / 'ws', call_fn=w.call)
        log = json.loads((w.dir / 'ws' / 'log.json').read_text())
        self.assertAlmostEqual(log[-1]['cost_usd'], 0.12)

    def test_an_interrupted_proposer_makes_the_final_cost_unknown(self):
        w = World([])

        def interrupted(backend, *args):
            if backend['argv'][0] == 'proposer':
                raise KeyboardInterrupt()
            return w.call(backend, *args)
        with self.assertRaises(KeyboardInterrupt):
            im.campaign(w.store, w.campaign, w.dir / 'ws', call_fn=interrupted)
        log = json.loads((w.dir / 'ws' / 'log.json').read_text())
        self.assertTrue(log[-1]['cost_unknown'])
        self.assertIsNone(log[-1]['cost_usd'])

    def test_unknown_proposer_cost_is_reported_as_unknown(self):
        w = World([edit(BASE_PROMPT + ' Be thorough.')])
        camp = json.loads(w.campaign.read_text())
        # Claude-style fake backends satisfy the known-cost declaration without running a CLI.
        w.campaign.write_text(json.dumps({**camp, 'proposer': 'claude:proposer', 'backend': 'claude:reviewer',
                                         'graders': ['claude:grader-a'], 'unpriced_call_usd': None}))

        def unpriced(backend, *args):
            text, usage = w.call(backend, *args)
            return text, None if backend['argv'][0] == 'proposer' else (usage or dict(USAGE))
        with self.assertRaisesRegex(ValueError, 'unpriced_call_usd'):
            im.campaign(w.store, w.campaign, w.dir / 'ws', call_fn=unpriced)
        self.assertFalse(any(r['kind'] == 'prereg' for r in ev.ledger(w.store)))

    def test_bad_proposals_are_screened_and_a_general_gain_becomes_the_finalist(self):
        two_changes = {**edit(BASE_PROMPT + ' x'), 'plan': {'version': 1, 'kind': 'review', 'role': 'r2',
                                                           'passes': [{'id': 'review', 'prompt': 'review.md', 'reply': 'findings'}]}}
        copies_gold = edit(BASE_PROMPT + ' Check whether the parser drops the last record when the input ends without a trailing newline.')
        good = edit(BASE_PROMPT + ' For each defect, state the failing input first.')
        w = World([two_changes, copies_gold, good])
        log = im.campaign(w.store, w.campaign, w.dir / 'ws', call_fn=w.call)
        self.assertEqual([e.get('result') for e in log[:3]], ['rejected', 'rejected', 'validation pass'])
        self.assertIn('exactly one change', log[0]['why'][0])
        self.assertIn('answer-key text', log[1]['why'][0])
        self.assertGreater(log[2]['dev']['U_gain'], 0)
        validation = json.loads((w.store / 'splits.json').read_text())['validation']
        for seen in w.proposer_saw:
            self.assertEqual(ev.leaked(w.store, seen), [])
            self.assertFalse(any(f'MATERIAL-{cid}' in seen for cid in validation))
            self.assertNotIn('src/app.py', seen)  # answer-key file paths never reach the improver
        self.assertEqual(ev.ledger(w.store)[-1]['kind'], 'campaign')

    def test_a_change_without_a_dev_gain_never_reaches_validation(self):
        w = World([edit(BASE_PROMPT + ' Be thorough.')] * 3)
        log = im.campaign(w.store, w.campaign, w.dir / 'ws', call_fn=w.call)
        self.assertTrue(all(e.get('result') == 'rejected' for e in log[:3]))
        self.assertFalse(any(r['kind'] == 'prereg' and r['split'] == 'validation' for r in ev.ledger(w.store)))


class ScreenTest(unittest.TestCase):
    def test_invalid_campaign_limits_are_rejected_before_spending(self):
        original = json.loads(self.w.campaign.read_text())
        for fields in ({'name': '../escape'}, {'max_proposals': -1}, {'dev_sample': True},
                       {'budget_usd': float('nan')}, {'max_prompt_growth': float('inf')}, {'allowance': [-1, 0.4]}):
            self.w.campaign.write_text(json.dumps({**original, **fields}))
            with self.subTest(fields=fields), self.assertRaises(ValueError):
                im.load_campaign(self.w.campaign)

    def test_an_invalid_plan_does_not_leave_a_screen_directory_behind(self):
        real, created = im.backends.private_tempdir, []

        @contextlib.contextmanager
        def track(*args, **kwargs):  # the private temporary folder, on every platform
            with real(*args, **kwargs) as path:
                created.append(Path(path))
                yield path
        plan, _ = im.plan_files(self.camp['incumbent'])
        plan['passes'][0]['id'] = 'invalid id'
        with mock.patch.object(im.backends, 'private_tempdir', track):
            self.assertIn('invalid', self.problems({'hypothesis': 'h', 'plan': plan}))
        self.assertTrue(created)
        self.assertFalse(any(p.exists() for p in created))

    def test_design_plans_cannot_replace_a_review_plan(self):
        plan, _ = im.plan_files(self.camp['incumbent'])
        plan['kind'] = 'design'
        plan['passes'][0]['reply'] = 'proposal'
        self.assertIn('only propose a review plan', self.problems({'hypothesis': 'h', 'plan': plan}))

    def setUp(self):
        self.w = World([])
        self.camp = im.load_campaign(self.w.campaign)
        self.dev = ev.split_cases(self.w.store, 'dev')

    def problems(self, proposal):
        return ' | '.join(im.screen(self.w.store, proposal, self.camp['incumbent'], self.dev, self.camp))

    def test_wrapped_answer_key_text_and_a_huge_role_are_caught(self):
        wrapped = ' '.join('the parser drops the last record when the input ends without a trailing newline'.split())
        wrapped = '\n'.join(' '.join(wrapped.split()[k:k + 4]) for k in range(0, 14, 4))
        self.assertIn('answer-key text', self.problems(edit(BASE_PROMPT + '\n' + wrapped)))
        plan, _ = im.plan_files(self.camp['incumbent'])
        self.assertIn('grow', self.problems({'hypothesis': 'h', 'plan': {**plan, 'role': 'x' * 100_000}}))

    def test_zero_width_characters_and_malformed_plans_are_handled(self):
        hidden = 'the par\u200bser drops the last record when the in\u200bput ends without a trailing newline'
        self.assertIn('answer-key text', self.problems(edit(BASE_PROMPT + ' ' + hidden)))
        self.assertIn('malformed', self.problems({'hypothesis': 'h', 'plan': {'passes': None}}))
        self.assertIn('malformed', self.problems({'hypothesis': 'h', 'plan': {'passes': [{'prompt': ['x']}]}}))

    def test_a_failed_proposer_call_is_still_charged(self):
        from harness import CallError
        w = self.w
        calls = []

        def failing(backend, system, message, timeout_s, model=None):
            if backend['argv'][0] == 'proposer':
                calls.append(1)
                raise CallError('rate limited', {'cost_usd': 0.02})
            return w.call(backend, system, message, timeout_s, model)
        log = im.campaign(w.store, w.campaign, w.dir / 'ws', call_fn=failing)
        lines = [json.loads(line) for line in (w.dir / 'ws' / 'proposer-charges.jsonl').read_text().splitlines()]
        charged = [c for c in lines if c['status'] == 'settled']
        self.assertEqual(len(charged), len(calls))
        self.assertTrue(all(c['cost_usd'] == 0.02 and c['failed'] for c in charged))
        self.assertGreaterEqual(log[-1]['cost_usd'], 0.02 * len(calls))

    def test_a_proposer_timeout_is_charged(self):
        import subprocess
        w = self.w

        def slow(backend, system, message, timeout_s, model=None):
            if backend['argv'][0] == 'proposer':
                raise subprocess.TimeoutExpired('proposer', 900)
            return w.call(backend, system, message, timeout_s, model)
        im.campaign(w.store, w.campaign, w.dir / 'ws', call_fn=slow)
        lines = [json.loads(x) for x in (w.dir / 'ws' / 'proposer-charges.jsonl').read_text().splitlines()]
        settled = [x for x in lines if x['status'] == 'settled']
        self.assertTrue(settled and all(x['failed'] and x['cost_usd'] == 0.0 for x in settled))
        self.assertEqual(sum(x['status'] == 'started' for x in lines), len(settled))

    def test_a_spent_budget_stops_the_campaign_before_any_run(self):
        camp = json.loads(self.w.campaign.read_text())
        self.w.campaign.write_text(json.dumps({**camp, 'budget_usd': 0}))
        with self.assertRaisesRegex(ValueError, 'budget'):
            im.campaign(self.w.store, self.w.campaign, self.w.dir / 'ws', call_fn=self.w.call)
        self.assertFalse(any(r['kind'] == 'prereg' for r in ev.ledger(self.w.store)))

    def test_rules(self):
        self.assertEqual(self.problems(edit(BASE_PROMPT + ' State the failing input first.')), '')
        self.assertIn('does not use', self.problems({'hypothesis': 'h', 'prompts': {'grade.md': 'Grade kindly.'}}))
        self.assertIn('grow', self.problems(edit(BASE_PROMPT * 3)))
        self.assertIn('names a file', self.problems(edit(BASE_PROMPT + ' Look hard at `src/io.py:` writes.')))
        many = {'hypothesis': 'h', 'plan': {'version': 1, 'kind': 'review', 'role': 'r', 'passes': [
            {'id': f'p{k}', 'prompt': 'review.md', 'reply': 'findings'} for k in range(4)] + [
            {'id': 'k', 'prompt': 'review.md', 'reply': 'verdicts', 'batch': 3}]}}
        self.assertNotIn('calls', self.problems(many))  # no call cap: the cost allowance prices extra calls
        with self.assertRaisesRegex(ev.EvalError, 'contamination'):
            self.problems(edit(BASE_PROMPT + ' canary' + '0' * 27 + '5'))


if __name__ == '__main__':
    unittest.main()
