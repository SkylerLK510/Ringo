"""Offline checks of evaluate.py: a fake case pool, fake reviewers and fake graders; no model calls."""
import json
import math
import os
import re
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import evaluate as ev  # noqa: E402
from tests.support import echo_cli  # noqa: E402

USAGE = {'input': 1, 'cache_write': 10, 'cache_read': 0, 'output': 5, 'cost_usd': 0.01}
FAMILIES = {'grader-claude': 'claude', 'grader-codex': 'codex'}  # fake graders standing in for two provider families
_patch = None
_original_identity = ev.identity

def fake_identity(name, store):
    virtual = {'good', 'base', 'broken', 'reviewer', 'proposer', 'grader-claude', 'grader-codex', 'grader-a', 'grader-x', 'fake-grader-a', 'fake-grader-b'}
    if ev.harness.backend(name)['argv'][0] in virtual:
        with mock.patch.object(ev.shutil, 'which', return_value=sys.executable):
            result = _original_identity(name, store)
    else:
        result = _original_identity(name, store)
    result['family'] = FAMILIES.get(result['argv'][0], result['family'])
    return result



def setUpModule():
    """Families come from the real executable; these fakes are mapped to two families so the tests don't depend
    on which CLIs this machine has installed. Everything else in identity() is the real code."""
    global _patch
    ev.real_identity = _original_identity
    _patch = mock.patch.object(ev, 'identity', fake_identity)
    _patch.start()


def tearDownModule():
    _patch.stop()


def fake_call(backend, system, message, timeout_s, model=None):
    """Reviewers: 'good' names G1 and G2, 'base' names G1; graders credit every G-id a review names."""
    who = backend['argv'][0]
    if 'grader' in who:
        ids = re.findall(r'=== Review (r-[0-9a-f]+) ===', message)
        bodies = re.split(r'=== Review r-[0-9a-f]+ ===', message)[1:]
        grades = {rid: {'found': sorted(set(re.findall(r'G\d+', body))), 'invalid': [], 'valid_extra': [], 'style': [],
                        'decoy_flagged': []} for rid, body in zip(ids, bodies)}
        return json.dumps(grades), None
    found = {'good': 'G1 and G2', 'base': 'G1', 'broken': None}[who]
    if found is None:
        raise ValueError('reviewer crashed')
    return f'What this change does: x.\n\n### 1. Defects {found} (`a.py:1`)\n- Problem: p', dict(USAGE)


class Store:
    def __init__(self, groups=12, fractions=(0, 0, 1), policy=None):
        self.dir = Path(tempfile.mkdtemp())
        unittest.addModuleCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        pool = {}
        for k in range(groups):
            cid = f'case{k}'
            d = self.dir / 'cases' / cid
            d.mkdir(parents=True)
            (d / 'material.txt').write_text(f'=== change.diff ===\n+ line {k}\n', encoding='utf-8')
            (d / 'gold.md').write_text(f'CANARY canary{k:028d}\n- **G1** first\n- **G2** second\n', encoding='utf-8')
            pool[cid] = {'group': f'owner/repo{k}', 'source': 'test', 'language': 'Python', 'created': '2026-10-01'}
        (self.dir / 'cases.json').write_text(json.dumps({'cases': pool}), encoding='utf-8')
        (self.dir / 'canaries.json').write_text(json.dumps({f'case{k}': f'canary{k:028d}' for k in range(groups)}))
        ev.split(self.dir, seed=3, fractions=fractions, new_policy=policy)

    def spec(self, **over):
        spec = {'name': 'exp', 'split': 'test', 'arms': {'cand': {'plan': None, 'backend': 'good'},
                                                        'base': {'plan': None, 'backend': 'base'}},
                'candidate': 'cand', 'comparators': ['base'], 'graders': ['grader-claude', 'grader-codex'],
                'min_groups': 10, 'bootstrap': 300, 'allow_tool_backends': True, **over}
        path = self.dir / f"spec-{len(list(self.dir.glob('spec-*')))}.json"
        path.write_text(json.dumps(spec), encoding='utf-8')
        return path

    def full(self, **over):
        d = ev.freeze(self.dir, self.spec(**over))
        ev.run(self.dir, d, parallel=4, call_fn=fake_call)
        ev.grade(self.dir, d, call_fn=fake_call)
        return d


class LedgerAndSplitTest(unittest.TestCase):
    def test_deleting_or_emptying_the_whole_ledger_is_detected_by_its_head(self):
        for remove in (False, True):
            s = Store(groups=1)
            path = s.dir / 'ledger.jsonl'
            if remove:
                path.unlink()
            else:
                path.write_text('')
            with self.subTest(remove=remove), self.assertRaisesRegex(ev.EvalError, 'head'):
                ev.ledger(s.dir)

    def test_invalid_policy_and_fractions_are_refused_before_writing(self):
        s = Store(groups=1)
        original = (s.dir / 'splits.json').read_text()
        for fractions in ((1, 0), (0, 0, 2), (float('nan'), 0, 1), (-1, 1, 1)):
            with self.subTest(fractions=fractions), self.assertRaises(ev.EvalError):
                ev.split(s.dir, resplit=True, fractions=fractions)
        self.assertEqual((s.dir / 'splits.json').read_text(), original)
        for policy in ({'alpha': 0.9}, {'alpha': float('nan')}, {'max_validation_submissions': True}):
            with tempfile.TemporaryDirectory() as d:
                Path(d, 'cases.json').write_text('{"cases": {}}')
                with self.subTest(policy=policy), self.assertRaises(ev.EvalError):
                    ev.split(d, new_policy=policy)
                self.assertFalse(Path(d, 'splits.json').exists())

    def test_case_names_cannot_escape_the_store(self):
        s = Store(groups=1)
        (s.dir / 'cases.json').write_text(json.dumps({'cases': {'../escape': {'group': 'g'}}}))
        with self.assertRaisesRegex(ev.EvalError, 'safe file name'):
            ev.cases(s.dir)

    def test_groups_stay_together_and_seen_cases_are_forced_into_dev(self):
        s = Store(fractions=(0.2, 0.3, 0.5))
        splits = json.loads((s.dir / 'splits.json').read_text())
        self.assertEqual(sorted(sum((splits[k] for k in ev.SPLITS), [])), sorted(f'case{k}' for k in range(12)))
        with self.assertRaisesRegex(ev.EvalError, 'already exist'):
            ev.split(s.dir)
        ev.record(s.dir, 'exposure', digest='x', split='test', cases=['case5'])
        self.assertIn('case5', ev.split(s.dir, seed=9, resplit=True)['dev'])

    def test_an_edited_ledger_stops_everything(self):
        s = Store()
        ev.record(s.dir, 'note', text='second line')
        path = s.dir / 'ledger.jsonl'
        original = path.read_text()
        path.write_text(original.replace('"split"', '"spl1t"', 1))           # an earlier line
        with self.assertRaisesRegex(ev.EvalError, 'broken'):
            ev.ledger(s.dir)
        path.write_text(original.replace('second line', 'edited line'))     # the last line
        with self.assertRaisesRegex(ev.EvalError, 'head'):
            ev.ledger(s.dir)
        path.write_text(original.splitlines(keepends=True)[0])              # a removed last line
        with self.assertRaisesRegex(ev.EvalError, 'head'):
            ev.ledger(s.dir)


class FreezeTest(unittest.TestCase):
    def test_guardrail_parameters_and_arm_names_are_validated(self):
        s = Store()
        for over in ({'min_groups': 2}, {'min_groups': True}, {'bootstrap': 0},
                     {'max_calls_per_review': 4}, {'seed': []}, {'kind': 'design'},
                     {'margins': {'U': -1, 'W': 0.25}}, {'margins': {'U': 0.5, 'W': float('nan')}},
                     {'max_cost_ratio': float('inf')}, {'min_saving': 1}, {'graders': ['grader-claude'] * 2},
                     {'arms': {'../escape': {'backend': 'good'}}},
                     {'arms': {'cand.v2': {'backend': 'good'}}}):
            with self.subTest(over=over), self.assertRaises(ev.EvalError):
                ev.load_spec(s.spec(**over))
        self.assertFalse(any(r['kind'] == 'prereg' for r in ev.ledger(s.dir)))
        with self.assertRaisesRegex(ev.EvalError, 'max_calls_per_review was removed'):
            ev.load_spec(s.spec(max_calls_per_review=4))

    def test_any_changed_input_invalidates_the_experiment(self):
        s = Store()
        d = ev.freeze(s.dir, s.spec())
        (s.dir / 'cases' / 'case0' / 'gold.md').write_text('- **G1** edited\n', encoding='utf-8')
        with self.assertRaisesRegex(ev.EvalError, 'changed since freeze'):
            ev.run(s.dir, d, call_fn=fake_call)

    def test_a_changed_prompt_invalidates_the_experiment(self):
        s = Store()
        prompts = s.dir / 'prompts'
        prompts.mkdir()
        (prompts / 'mine.md').write_text('Review it.', encoding='utf-8')
        plan = s.dir / 'plan.json'
        plan.write_text(json.dumps({'version': 1, 'kind': 'review', 'role': 'r', 'prompts_dir': 'prompts',
                                    'passes': [{'id': 'r', 'prompt': 'mine.md', 'reply': 'findings'}]}))
        d = ev.freeze(s.dir, s.spec(arms={'cand': {'plan': str(plan), 'backend': 'good'}, 'base': {'plan': None, 'backend': 'base'}}))
        (prompts / 'mine.md').write_text('Review it harder.', encoding='utf-8')
        with self.assertRaisesRegex(ev.EvalError, 'changed since freeze'):
            ev.checked(s.dir, d)

    def test_test_split_needs_two_grader_families_and_one_open_experiment(self):
        s = Store()
        for graders in (['grader-claude'], ['fake-grader-a', 'fake-grader-b'], ['claude:grader-x', 'grader-claude']):
            with self.subTest(graders=graders), self.assertRaisesRegex(ev.EvalError, 'two provider families'):
                ev.freeze(s.dir, s.spec(graders=graders))
        ev.freeze(s.dir, s.spec())
        with self.assertRaisesRegex(ev.EvalError, 'another test experiment is open'):
            ev.freeze(s.dir, s.spec(runs=3))

    def test_validation_submissions_are_capped_per_campaign_by_the_store_policy(self):
        s = Store(fractions=(0, 1, 0), policy={'max_validation_submissions': 2})
        for runs in (1, 2):
            ev.freeze(s.dir, s.spec(split='validation', campaign='c1', runs=runs))
        with self.assertRaisesRegex(ev.EvalError, 'used its 2 validation submissions'):
            ev.freeze(s.dir, s.spec(split='validation', campaign='c1', runs=3))
        with self.assertRaisesRegex(ev.EvalError, 'unknown spec keys'):
            ev.freeze(s.dir, s.spec(split='validation', campaign='c2', max_validation_submissions=99))

    def test_a_resplit_cannot_loosen_the_policy(self):
        s = Store(policy={'alpha': 0.05})
        ev.split(s.dir, seed=5, resplit=True, new_policy={'alpha': 0.5, 'max_validation_submissions': 99})
        self.assertEqual(ev.policy(s.dir), ev.POLICY_DEFAULTS)


class CheckerBackendTest(unittest.TestCase):
    """An arm's checker_backend runs its plan's verdicts pass; it is checked, frozen and traced like any backend."""
    def checked_plan(self, s):
        prompts = s.dir / 'prompts'
        prompts.mkdir(exist_ok=True)
        for name in ('r.md', 'k.md'):
            (prompts / name).write_text(f'INSTRUCTION {name}', encoding='utf-8')
        path = s.dir / 'checked.json'
        path.write_text(json.dumps({'version': 1, 'kind': 'review', 'role': 'r', 'prompts_dir': 'prompts',
                                    'passes': [{'id': 'r', 'prompt': 'r.md', 'reply': 'findings'},
                                               {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}]}))
        return str(path)

    def arms(self, s, checker):
        return {'cand': {'plan': self.checked_plan(s), 'backend': 'good', 'checker_backend': checker},
                'base': {'plan': None, 'backend': 'base'}}

    def test_bad_checker_backends_are_refused_at_freeze(self):
        s = Store(groups=2, fractions=(1, 0, 0))
        for arms, error in [({'cand': {'plan': None, 'backend': 'good', 'checker_backend': 'reviewer'}}, 'verdicts pass'),
                            ({'cand': {'plan': None, 'backend': 'good', 'checker_backend': ' '}}, 'checker_backend')]:
            with self.subTest(error=error), self.assertRaisesRegex(ev.EvalError, error):
                ev.freeze(s.dir, s.spec(split='dev', arms=arms, comparators=[], graders=['grader-claude']))
        with self.assertRaisesRegex(ValueError, "may read local files: .*'codex'"):
            ev.freeze(s.dir, s.spec(split='dev', arms=self.arms(s, 'codex'), graders=['claude'], allow_tool_backends=False))

    def test_checker_is_frozen_and_runs_the_verdicts_pass(self):
        s = Store(groups=2, fractions=(1, 0, 0))
        names = []
        def recording(name, store):
            names.append(name)
            return fake_identity(name, store)
        with mock.patch.object(ev, 'identity', recording):
            d = ev.freeze(s.dir, s.spec(split='dev', arms=self.arms(s, 'reviewer'), graders=['grader-claude'], runs=1))
        self.assertIn('reviewer', names)
        def call(b, system, message, timeout_s, model=None):
            return ('r.1 KEEP', dict(USAGE)) if b['name'] == 'reviewer' else fake_call(b, system, message, timeout_s, model)
        ev.run(s.dir, d, call_fn=call)
        traces = sorted((s.dir / 'runs' / d / 'out').glob('*/cand.*/trace/trace.jsonl'))
        self.assertEqual(len(traces), 2)
        for t in traces:
            ok = {r['step']: r['backend'] for r in map(json.loads, t.read_text().splitlines()) if r['status'] == 'ok'}
            self.assertEqual(ok, {'r': 'good', 'k': 'reviewer'})


class RunGradeDecideTest(unittest.TestCase):
    def test_failed_grades_cannot_be_selectively_retried(self):
        s = Store(groups=1, fractions=(1, 0, 0))
        d = ev.freeze(s.dir, s.spec(split='dev', runs=1))
        ev.run(s.dir, d, call_fn=fake_call)
        calls = []

        def malformed(*args):
            calls.append(1)
            return '{}', dict(USAGE)
        ev.grade(s.dir, d, call_fn=malformed)
        first_cost = ev.cost(s.dir, d)
        self.assertEqual(len(calls), 4)  # two attempts for each grader
        ev.grade(s.dir, d, call_fn=fake_call)
        self.assertEqual(ev.cost(s.dir, d), first_cost)
        self.assertFalse(list((s.dir / 'runs' / d / 'grades').glob('*/case0.json')))

    def test_a_crash_after_two_settled_grades_does_not_grant_more_attempts(self):
        s = Store(groups=1, fractions=(1, 0, 0))
        d = ev.freeze(s.dir, s.spec(split='dev', runs=1, graders=['grader-claude']))
        ev.run(s.dir, d, call_fn=fake_call)
        log = s.dir / 'runs' / d / 'grade-charges.jsonl'
        log.write_text(''.join(json.dumps({'case': 'case0', 'grader': 'grader-claude', 'attempt': attempt,
                                          'status': 'settled', 'cost_usd': 0.01}) + '\n' for attempt in (1, 2)))
        calls = []
        ev.grade(s.dir, d, call_fn=lambda *a: calls.append(1) or fake_call(*a))
        self.assertEqual(calls, [])

    def test_recall_is_normalized_by_each_cases_number_of_gold_defects(self):
        s = Store(groups=10)
        for cid in ev.split_cases(s.dir, 'test'):
            if int(cid[4:]) % 2 == 0:
                (s.dir / 'cases' / cid / 'gold.md').write_text('\n'.join(f'- **G{k}** defect' for k in range(1, 101)))
            else:
                (s.dir / 'cases' / cid / 'gold.md').write_text('- **G1** defect')

        def reviewers(backend, system, message, timeout_s, model=None):
            if 'grader' in backend['argv'][0]:
                return fake_call(backend, system, message, timeout_s, model)
            k = int(re.search(r'\+ line (\d+)', system).group(1))
            ids = ([f'G{n}' for n in range(1, 11)] if k % 2 == 0 else []) if backend['argv'][0] == 'good' \
                else ([] if k % 2 == 0 else ['G1'])
            return '### 1. Defects ' + ', '.join(ids), dict(USAGE)
        d = ev.freeze(s.dir, s.spec(runs=1))
        ev.run(s.dir, d, call_fn=reviewers)
        ev.grade(s.dir, d, call_fn=reviewers)
        report = ev.score(s.dir, d)
        entry = report['graders']['grader-claude']['vs']['base']
        self.assertAlmostEqual(entry['T']['mean'], 4.5)
        self.assertAlmostEqual(entry['R']['mean'], -0.45)
        self.assertTrue(any('recall gain' in why for _, why in ev.gates(ev.checked(s.dir, d), report)))

    def test_a_real_gain_passes_and_the_test_pool_is_retired(self):
        s = Store()
        d = s.full()
        result, why = ev.decide(s.dir, d)
        self.assertEqual((result, why), ('pass', []))
        with self.assertRaisesRegex(ev.EvalError, 'retired'):
            ev.freeze(s.dir, s.spec(runs=1))
        with self.assertRaisesRegex(ev.EvalError, 'already decided'):
            ev.decide(s.dir, d)

    def test_no_gain_fails(self):
        s = Store()
        d = s.full(arms={'cand': {'plan': None, 'backend': 'base'}, 'base': {'plan': None, 'backend': 'base'}})
        result, why = ev.decide(s.dir, d)
        self.assertEqual(result, 'fail')
        self.assertTrue(any('lower bound of U gain' in w for w in why))

    def test_a_failed_slot_or_too_few_groups_is_inconclusive(self):
        s = Store()
        d = s.full(arms={'cand': {'plan': None, 'backend': 'broken'}, 'base': {'plan': None, 'backend': 'base'}})
        result, why = ev.decide(s.dir, d)
        self.assertEqual(result, 'inconclusive')
        self.assertTrue(any(w.startswith('incomplete') for w in why))
        small = Store(groups=6)
        result, why = ev.decide(small.dir, small.full())
        self.assertEqual(result, 'inconclusive')
        self.assertTrue(any('repository groups' in w for w in why))

    def test_grading_is_blind_and_the_key_lives_outside_the_run(self):
        s = Store(groups=10)
        seen = []

        def spy(backend, system, message, timeout_s, model=None):
            if 'grader' in backend['argv'][0]:
                seen.append(message)
            return fake_call(backend, system, message, timeout_s)
        d = ev.freeze(s.dir, s.spec())
        ev.run(s.dir, d, call_fn=fake_call)
        ev.grade(s.dir, d, call_fn=spy)
        self.assertTrue(seen)
        for message in seen:
            self.assertNotRegex(message, r'\b(cand|base)\b')
        key = json.loads((s.dir / 'keys' / f'{d}.json').read_text())
        self.assertEqual(len(key), 10 * 2 * 2)
        self.assertFalse(list((s.dir / 'runs' / d).rglob('keys')))

    def test_failed_slots_are_never_rerun(self):
        s = Store(groups=10)
        d = ev.freeze(s.dir, s.spec(arms={'cand': {'plan': None, 'backend': 'broken'}, 'base': {'plan': None, 'backend': 'base'}}))
        self.assertEqual(len(ev.run(s.dir, d, call_fn=fake_call)), 20)
        calls = []
        ev.run(s.dir, d, call_fn=lambda *a: calls.append(a) or fake_call(*a))
        self.assertEqual(calls, [])


class ReviewFindingsTest(unittest.TestCase):
    """One regression test per finding of the cross-model review."""

    def test_dev_cases_never_move_into_test_on_a_resplit(self):
        s = Store(fractions=(1, 0, 0))
        dev = set(json.loads((s.dir / 'splits.json').read_text())['dev'])
        doc = ev.split(s.dir, seed=4, fractions=(0, 0, 1), resplit=True)
        self.assertEqual(set(doc['dev']), dev)
        self.assertEqual(doc['test'], [])

    def test_concurrent_test_freezes_cannot_both_open_the_pool(self):
        import threading
        s = Store()
        specs, results, barrier = [s.spec(runs=1), s.spec(runs=2)], [], threading.Barrier(2)

        def go(path):
            barrier.wait()
            try:
                results.append(ev.freeze(s.dir, path))
            except ev.EvalError as exc:
                results.append(exc)
        threads = [threading.Thread(target=go, args=(p,)) for p in specs]
        [t.start() for t in threads]
        [t.join() for t in threads]
        self.assertEqual(sum(isinstance(r, str) for r in results), 1)

    def test_changed_repository_groups_invalidate_the_experiment(self):
        s = Store()
        d = ev.freeze(s.dir, s.spec())
        pool = json.loads((s.dir / 'cases.json').read_text())
        for meta in pool['cases'].values():
            meta['group'] = 'owner/one-repo'
        (s.dir / 'cases.json').write_text(json.dumps(pool))
        with self.assertRaisesRegex(ev.EvalError, 'changed since freeze'):
            ev.checked(s.dir, d)

    def test_grading_needs_a_finished_run_and_full_coverage(self):
        s = Store(groups=10)
        d = ev.freeze(s.dir, s.spec())
        with self.assertRaisesRegex(ev.EvalError, 'not finished'):
            ev.grade(s.dir, d, call_fn=fake_call)
        ev.run(s.dir, d, call_fn=fake_call)
        ev.grade(s.dir, d, call_fn=fake_call)
        grades = next((s.dir / 'runs' / d / 'grades').glob('*/case0.json'))
        partial = json.loads(grades.read_text())
        partial.pop(next(iter(partial)))
        grades.write_text(json.dumps(partial))
        result, why = ev.decide(s.dir, d)
        self.assertEqual(result, 'inconclusive')
        self.assertTrue(any('missing grades' in w for w in why))

    def test_an_edited_split_file_is_refused(self):
        s = Store(fractions=(1, 0, 0))
        doc = json.loads((s.dir / 'splits.json').read_text())
        doc['test'], doc['dev'] = doc['dev'], []
        (s.dir / 'splits.json').write_text(json.dumps(doc))
        with self.assertRaisesRegex(ev.EvalError, 'does not match the ledger'):
            ev.freeze(s.dir, s.spec())

    def test_a_budget_governs_every_attempt_and_needs_known_costs(self):
        s = Store(groups=10)
        with self.assertRaisesRegex(ev.EvalError, 'report cost|unpriced_call_usd'):
            ev.freeze(s.dir, s.spec(budget_usd=1.0))                     # fake backends report no style=claude cost
        d = ev.freeze(s.dir, s.spec(budget_usd=0.05, unpriced_call_usd=0.01))
        ev.run(s.dir, d, call_fn=fake_call)
        ev.grade(s.dir, d, call_fn=fake_call)
        spent, unknown = ev.cost(s.dir, d)
        self.assertFalse(unknown)
        self.assertLessEqual(spent, 0.05 + 0.02)                          # at most the attempts already running
        result, why = ev.decide(s.dir, d)
        self.assertEqual(result, 'inconclusive')

    def test_two_runs_of_one_experiment_cannot_overlap(self):
        s = Store(groups=10)
        d = ev.freeze(s.dir, s.spec())
        with ev.exclusive(s.dir / 'runs' / d / '.run.lock', 'run'):
            with self.assertRaisesRegex(ev.EvalError, 'in progress'):
                ev.run(s.dir, d, call_fn=fake_call)

    def test_resume_neither_forgets_nor_double_counts(self):
        s = Store(groups=10)
        d = ev.freeze(s.dir, s.spec(unpriced_call_usd=0.0))
        ev.run(s.dir, d, call_fn=fake_call)
        first = ev.cost(s.dir, d)[0]
        ev.run(s.dir, d, call_fn=fake_call)                                # nothing left to do
        self.assertEqual(ev.cost(s.dir, d)[0], first)
        self.assertEqual([r['cost_usd'] for r in ev.ledger(s.dir) if r['kind'] == 'run'][-1], 0)

    def test_an_experiment_with_no_reviews_is_inconclusive_not_a_crash(self):
        s = Store(groups=10)
        d = s.full(arms={'cand': {'plan': None, 'backend': 'broken'}, 'base': {'plan': None, 'backend': 'broken'}})
        self.assertEqual(ev.decide(s.dir, d)[0], 'inconclusive')

    def test_identity_hashes_scripts_and_only_runs_trusted_executables(self):
        s = Store()
        d = Path(tempfile.mkdtemp())
        marker, script = d / 'ran', d / 'impl.py'
        script.write_text(f'open({str(marker)!r}, "w").write("x")\n')
        name = f'{sys.executable} {script}'
        before = ev.real_identity(name, s.dir)
        self.assertFalse(marker.exists())                                 # never executed: nobody vouched for it
        self.assertEqual(before['family'], 'custom')
        script.write_text(f'open({str(marker)!r}, "w").write("y")\n')
        self.assertNotEqual(ev.real_identity(name, s.dir)['files'], before['files'])  # the script is frozen too

    def test_only_an_executable_a_person_vouched_for_gets_a_family(self):
        s = Store()
        d = Path(tempfile.mkdtemp())
        stub = echo_cli(d / 'claude', '9.9 (Claude Code)')
        self.assertEqual(ev.real_identity(stub, s.dir)['family'], 'custom')
        ev.trust(s.dir, stub, 'claude')
        if os.name == 'nt':
            # Trust does not exempt a batch wrapper from the native launch guard.
            with self.assertRaisesRegex(ValueError, 'call the underlying executable'):
                ev.real_identity(stub, s.dir)
        else:
            trusted = ev.real_identity(stub, s.dir)
            self.assertEqual((trusted['family'], trusted['version']), ('claude', '9.9 (Claude Code)'))
        echo_cli(d / 'claude', '9.9 (Claude Code)', note='changed')  # same name, other content
        self.assertEqual(ev.real_identity(stub, s.dir)['family'], 'custom')

    def test_an_interrupted_slot_has_an_unknown_cost_and_is_never_rerun(self):
        s = Store(groups=10)
        d = ev.freeze(s.dir, s.spec(unpriced_call_usd=0.0))
        cid = ev.split_cases(s.dir, 'test')[0]
        trace = s.dir / 'runs' / d / 'out' / cid / 'cand.1' / 'trace'
        trace.mkdir(parents=True)
        (trace / 'trace.jsonl').write_text(json.dumps({'step': 'review', 'attempt': 1, 'status': 'started'}) + '\n')
        self.assertIsNone(ev.slot_cost(trace.parent, 0.0))                # paid for, but how much is unknown
        self.assertTrue(ev.cost(s.dir, d)[1])
        ev.run(s.dir, d, call_fn=fake_call)
        self.assertTrue((trace.parent / 'failed.json').exists())
        self.assertFalse((trace.parent / 'result.md').exists())

    def test_a_failure_summary_cannot_hide_a_pending_attempt(self):
        where = Path(tempfile.mkdtemp())
        (where / 'trace').mkdir()
        (where / 'trace' / 'trace.jsonl').write_text(json.dumps({'step': 'r', 'attempt': 1, 'status': 'started'}) + '\n')
        (where / 'failed.json').write_text(json.dumps({'error': 'x', 'usage': {'calls': 1, 'cost_usd': 0.0, 'unpriced_calls': 0}}))
        self.assertIsNone(ev.slot_cost(where, 0.0))

    def test_an_interrupted_grader_attempt_is_unknown_and_never_repeated(self):
        s = Store(groups=10)
        d = ev.freeze(s.dir, s.spec(unpriced_call_usd=0.0))
        ev.run(s.dir, d, call_fn=fake_call)
        cid = ev.split_cases(s.dir, 'test')[0]
        log = s.dir / 'runs' / d / 'grade-charges.jsonl'
        log.write_text(json.dumps({'case': cid, 'grader': 'grader-claude', 'attempt': 1, 'status': 'started'}) + '\n')
        self.assertTrue(ev.cost(s.dir, d)[1])
        calls = []
        ev.grade(s.dir, d, call_fn=lambda b, *a: calls.append((b['argv'][0], a[1])) or fake_call(b, *a))
        self.assertFalse(any(who == 'grader-claude' and f'Review' in msg and cid in msg for who, msg in calls))
        self.assertTrue(list((s.dir / 'runs' / d / 'grades').glob(f'*/{cid}.failed')))

    def test_a_budgeted_decision_needs_every_cost_known(self):
        s = Store(groups=10)
        d = ev.freeze(s.dir, s.spec(budget_usd=100.0, unpriced_call_usd=0.0))
        ev.run(s.dir, d, call_fn=fake_call)
        ev.grade(s.dir, d, call_fn=fake_call)
        log = s.dir / 'runs' / d / 'grade-charges.jsonl'
        lines = log.read_text().splitlines()
        last = json.loads(lines[-1])
        lines[-1] = json.dumps({**last, 'cost_usd': None})                # the final grading charge is unknown
        log.write_text('\n'.join(lines) + '\n')
        result, why = ev.decide(s.dir, d)
        self.assertEqual(result, 'inconclusive')
        self.assertTrue(any('unknown' in w for w in why))

    def test_stores_inside_the_public_repository_are_refused(self):
        inside = Path(ev.HERE) / 'my-store'
        with self.assertRaisesRegex(ev.EvalError, 'outside the repository'):
            ev.outside_repo(inside, 'store')
        with mock.patch('sys.stderr'):
            self.assertEqual(ev.main(['split', '--store', str(inside)]), 2)
        self.assertFalse(inside.exists())

    def test_a_non_object_grade_is_malformed_not_a_crash(self):
        with self.assertRaisesRegex(ValueError, 'not an object'):
            ev.parse_grades('{"r-1": null}', ['r-1'], ['G1'])

    def test_grades_need_exact_review_ids_and_all_required_fields(self):
        complete = {'found': ['G1'], 'invalid': [], 'valid_extra': [], 'style': [], 'decoy_flagged': []}
        with self.assertRaisesRegex(ValueError, 'exactly one'):
            ev.parse_grades(json.dumps({'r-1': complete, 'r-extra': complete}), ['r-1'], ['G1'])
        for field in ('found', 'invalid'):
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, 'must contain'):
                ev.parse_grades(json.dumps({'r-1': {k: v for k, v in complete.items() if k != field}}), ['r-1'], ['G1'])

    def test_an_edited_backend_wrapper_invalidates_the_experiment(self):
        s = Store()
        wrapper = echo_cli(s.dir / 'my-reviewer', 'one')
        d = ev.freeze(s.dir, s.spec(arms={'cand': {'plan': None, 'backend': wrapper},
                                           'base': {'plan': None, 'backend': 'base'}}))
        echo_cli(s.dir / 'my-reviewer', 'two')
        with self.assertRaisesRegex(ev.EvalError, 'changed since freeze'):
            ev.checked(s.dir, d)


class StatsTest(unittest.TestCase):
    def test_t_quantiles_match_closed_forms_in_small_tails(self):
        for p in (0.01, 0.975, 0.999999):
            tail = min(p, 1 - p)
            cauchy = (1 if p > 0.5 else -1) / math.tan(math.pi * tail)
            self.assertAlmostEqual(ev.t_quantile(p, 1) / cauchy, 1, places=10)
            df2 = (2 * p - 1) / math.sqrt(2 * p * (1 - p))
            self.assertAlmostEqual(ev.t_quantile(p, 2) / df2, 1, places=10)

    def test_alpha_spending_t_tail_matches_independent_integration(self):
        p = 1 - 1e-10
        t = ev.t_quantile(p, 9)
        # For df=9, t=3*cot(theta) turns the upper tail into
        # 128/(35*pi) * integral_0^theta sin(x)^8 dx. Simpson integration
        # checks the tail without using the production beta function.
        theta, n = math.atan(3 / t), 2048
        h = theta / n
        integral = h / 3 * (math.sin(theta) ** 8 + sum(
            (4 if k % 2 else 2) * math.sin(k * h) ** 8 for k in range(1, n)))
        tail = 128 / (35 * math.pi) * integral
        self.assertAlmostEqual(tail / (1 - p), 1, places=10)

    def test_t_quantiles_match_tables(self):
        for p, df, want in ((0.975, 10, 2.228), (0.95, 20, 1.725), (0.99, 9, 2.821), (0.975, 30, 2.042)):
            self.assertAlmostEqual(ev.t_quantile(p, df), want, places=2)

    def test_cluster_bound_by_hand(self):
        deltas, groups = [1.0, 3.0, 2.0, 2.0], ['a', 'a', 'b', 'c']   # mean 2; group residual sums 0, 0, 0
        self.assertAlmostEqual(ev.cluster_bound(deltas, groups, 0.05, 'lower'), 2.0)
        deltas, groups = [0.0, 2.0, 4.0], ['a', 'b', 'c']               # mean 2; se = sqrt(1.5 * 8) / 3
        se = (1.5 * 8) ** 0.5 / 3
        self.assertAlmostEqual(ev.cluster_bound(deltas, groups, 0.05, 'upper'), 2 + ev.t_quantile(0.95, 2) * se)
        self.assertIsNone(ev.cluster_bound([1.0, 2.0], ['a', 'a'], 0.05, 'lower'))

    def test_bootstrap_is_reproducible_and_brackets_the_mean(self):
        deltas, groups = [0.0, 1.0, 2.0, 3.0, 4.0], list('abcde')
        lo = ev.bootstrap_bound(deltas, groups, 0.05, 'lower', 2000, 7)
        self.assertEqual(lo, ev.bootstrap_bound(deltas, groups, 0.05, 'lower', 2000, 7))
        self.assertLess(lo, 2.0)
        self.assertGreater(ev.bootstrap_bound(deltas, groups, 0.05, 'upper', 2000, 7), 2.0)

    def test_alpha_is_spent_per_test_decision_and_never_reset(self):
        s = Store()
        self.assertAlmostEqual(ev.alpha(s.dir, 'test'), 0.025)
        ev.record(s.dir, 'decision', digest='x', split='test', result='fail')
        self.assertAlmostEqual(ev.alpha(s.dir, 'test'), 0.05 / 6)
        self.assertAlmostEqual(ev.alpha(s.dir, 'validation'), 0.05)

    def test_canaries_detect_leaked_gold(self):
        s = Store()
        self.assertEqual(ev.leaked(s.dir, 'notes ... canary' + '0' * 25 + '003 ...'), ['case3'])
        self.assertEqual(ev.leaked(s.dir, 'clean'), [])

if __name__ == '__main__':
    unittest.main()
