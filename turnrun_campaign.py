"""Gated recursive turn-policy experiments, with an offline reference executor.

Search state is never deployment. Live execution is deliberately unavailable until a native
subscription broker passes isolation preflight. This controller does not launch provider CLIs.
"""
import argparse
import copy
import hashlib
import json
import math
from pathlib import Path
import random
import statistics
import time
import unicodedata

from turnrun import CampaignLock, ledger_append, ledger_entries, outside_repo, sha256_file, usage_of

POLICY_DEFAULTS = {'instruction': '', 'handoff_instruction': '', 'handoff_at': 64000}
PHASES = ('controls', 'search', 'manual', 'confirmation')
BLOCKERS = ['native subscription broker is not certified',
            'fresh sealed scenarios and split sensitivity are not verified',
            'numeric live inference budget is not recorded',
            'live controls have not run', 'live campaigns cannot launch inside a Ringo child']


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':'),
                                     allow_nan=False).encode()).hexdigest()


def policy(value):
    if not isinstance(value, dict) or set(value) - POLICY_DEFAULTS.keys():
        raise ValueError('unknown editable policy field')
    result = {**POLICY_DEFAULTS, **value}
    for key in ('instruction', 'handoff_instruction'):
        text = result[key]
        if not isinstance(text, str) or len(text.encode()) > 4096 or any(
                unicodedata.category(c) == 'Cc' and c not in '\n\t' for c in text):
            raise ValueError(f'invalid {key}')
    if type(result['handoff_at']) is not int or not 16000 <= result['handoff_at'] <= 128000:
        raise ValueError('handoff_at must be an integer from 16000 through 128000')
    return result


def validate_plan(value):
    required = {'version', 'splits', 'clusters', 'coverage', 'baseline', 'degraded', 'manual',
                'budgets', 'run_cap', 'proposal_cap', 'margin', 'seconds', 'rounds',
                'repetitions', 'search_repetitions', 'seed', 'min_baseline', 'min_sensitivity',
                'min_gain', 'min_savings', 'quality_tolerance', 'cost_allowance'}
    if not isinstance(value, dict) or set(value) != required or value['version'] != 1:
        raise ValueError('plan fields do not match schema version 1')
    p = copy.deepcopy(value)
    if set(p['splits']) != {'dev', 'validation', 'holdout'}:
        raise ValueError('dev, validation and holdout splits are required')
    seen, cluster_sides = set(), {}
    for side, tasks in p['splits'].items():
        if not isinstance(tasks, list) or not tasks or len(set(tasks)) != len(tasks):
            raise ValueError('each split needs unique task IDs')
        for task in tasks:
            if not isinstance(task, str) or not task or task in seen:
                raise ValueError('a task belongs to exactly one split')
            seen.add(task)
            cluster = p['clusters'].get(task)
            if not isinstance(cluster, str) or not cluster:
                raise ValueError('each task needs a fix-family cluster')
            if cluster in cluster_sides and cluster_sides[cluster] != side:
                raise ValueError('a fix-family cluster crosses splits')
            cluster_sides[cluster] = side
    if set(p['clusters']) != seen:
        raise ValueError('cluster inventory differs from the splits')
    if not isinstance(p['coverage'], list) or set(p['coverage']) - POLICY_DEFAULTS.keys():
        raise ValueError('coverage must name tested policy fields')
    for key in ('baseline', 'degraded', 'manual'):
        p[key] = policy(p[key])
    if p['baseline'] == p['degraded']:
        raise ValueError('the negative control must differ from baseline')
    if set(p['budgets']) != set(PHASES) or any(type(n) is not int or n <= 0 for n in p['budgets'].values()):
        raise ValueError('positive, separate phase budgets are required')
    if p['budgets']['search'] != p['budgets']['manual']:
        raise ValueError('automatic and engineer-selected arms need equal admission ceilings')
    for key, low, high in [('run_cap', 1, 100000000), ('proposal_cap', 1, 10000000),
                          ('seconds', 1, 3600), ('rounds', 1, 2), ('repetitions', 3, 20),
                          ('search_repetitions', 2, 20), ('margin', 0, 100000000), ('seed', 0, 2**32-1)]:
        if type(p[key]) is not int or not low <= p[key] <= high:
            raise ValueError(f'invalid {key}')
    for key in ('min_baseline', 'min_sensitivity', 'min_gain', 'min_savings', 'quality_tolerance', 'cost_allowance'):
        n = p[key]
        if type(n) not in (int, float) or not math.isfinite(n) or not 0 <= n <= 1:
            raise ValueError(f'invalid {key}')
    if p['min_sensitivity'] <= 0 or p['min_gain'] <= 0 or p['min_savings'] <= 0:
        raise ValueError('sensitivity, worthwhile gain and savings must be positive')
    return p


def admission_schedule(value):
    """Size the complete finite schedule without inference or spending authorization."""
    p = validate_plan(value)
    dev = len(p['splits']['dev'])
    confirmation = len(p['splits']['validation']) + len(p['splits']['holdout'])
    counts = {'controls': 2 * dev * p['repetitions'],
              'search': 3 * dev * p['search_repetitions'] * p['rounds'],
              'manual': dev * p['search_repetitions'],
              'confirmation': 3 * confirmation * p['search_repetitions']}
    tokens = {phase: count * p['run_cap'] for phase, count in counts.items()}
    proposals = 2 * p['rounds']
    tokens['search'] += proposals * p['proposal_cap']
    minimum = {phase: n + p['margin'] for phase, n in tokens.items()}
    matched = max(minimum['search'], minimum['manual'])
    minimum.update(search=matched, manual=matched)
    return {'plan_sha256': digest(p), 'evaluation_attempts': counts, 'proposal_attempts': proposals,
            'maximum_attempts': sum(counts.values()) + proposals,
            'maximum_reservations': tokens, 'minimum_phase_ceilings': minimum,
            'declared_phase_ceilings': p['budgets'],
            'full_schedule_fits': all(p['budgets'][phase] >= n for phase, n in minimum.items()),
            'minimum_total_ceiling': sum(minimum.values()),
            'provider_calls': 0, 'spending_authorized': False,
            'limits': 'Admission includes cache tokens. A provider turn can overshoot its reservation.'}


def code_identity():
    root = Path(__file__).resolve().parent
    return {name: sha256_file(root / name) for name in ('turnrun_campaign.py', 'turnrun_isolation.py',
            'turnrun_broker.py', 'turnrun_certify.py',
            'turnrun.py', 'cli/src/turn_policy.rs', 'cli/src/bin/scenariorun.rs',
            'cli/src/bin/turnrun.rs', 'cli/src/lib.rs', 'cli/src/parse.rs')}


def demonstration_plan():
    """Synthetic IDs and caps for the offline demonstration, never an experiment protocol."""
    splits = {'dev': ['synthetic-D1', 'synthetic-D2', 'synthetic-D3'],
              'validation': ['synthetic-V1'], 'holdout': ['synthetic-H1']}
    return {'version': 1, 'splits': splits, 'clusters': {t: t for ts in splits.values() for t in ts},
        'coverage': ['instruction', 'handoff_instruction', 'handoff_at'], 'baseline': {},
        'degraded': {'instruction': 'synthetic degraded'}, 'manual': {'instruction': 'synthetic manual'},
        'budgets': {p: 100000 for p in PHASES}, 'run_cap': 1000, 'proposal_cap': 100,
        'margin': 1000, 'seconds': 30, 'rounds': 2, 'repetitions': 3, 'search_repetitions': 2,
        'seed': 12, 'min_baseline': 0.5, 'min_sensitivity': 0.2, 'min_gain': 0.1,
        'min_savings': 0.1, 'quality_tolerance': 0, 'cost_allowance': 0.1}


def offline_demonstration(campaign):
    """Exercise gates and recursion with predetermined synthetic observations. Zero inference."""
    def observation(tokens, **data):
        return {'outcome': 'done', 'synthetic': True, 'usage': {'input': tokens, 'cache_write': 0,
            'cache_read': 0, 'output': 0, 'counted': True, 'uncounted_calls': 0}, **data}
    def evaluate(selected, task, rep, cap, seconds):
        note = selected['instruction']
        cost = {'synthetic first': 70, 'synthetic second': 50, 'synthetic manual': 80}.get(note, 100)
        return observation(cost, passed=note != 'synthetic degraded', protected_pass=True)
    def propose(parent, history, round_no, sibling, cap):
        note = ('synthetic first' if round_no == 1 else 'synthetic second') if sibling == 1 else 'synthetic alternative'
        return observation(10, policy={**parent, 'instruction': note}, hypothesis='Synthetic state-machine exercise')
    campaign.controls(evaluate)
    campaign.search(evaluate, propose)
    campaign.confirm(evaluate)
    return {**campaign.status(), 'synthetic': True, 'provider_calls': 0,
            'limits': 'All observations and ledger token counters are simulated; no quality evidence.'}


class Campaign:
    def __init__(self, root):
        self.root = outside_repo(root, 'campaign')
        self.path = self.root / 'ledger.jsonl'
        self.plan = validate_plan(json.loads((self.root / 'plan.json').read_text()))
        self.verify()

    @classmethod
    def create(cls, root, plan):
        root = outside_repo(root, 'campaign')
        plan = validate_plan(plan)
        root.mkdir(parents=True, exist_ok=False)
        (root / 'plan.json').write_text(json.dumps(plan, sort_keys=True, indent=2) + '\n')
        ledger_append(root / 'ledger.jsonl', {'kind': 'prepared', 'plan_sha256': digest(plan),
            'code': code_identity(), 'live': False, 'blockers': BLOCKERS})
        return cls(root)

    def verify(self):
        entries = ledger_entries(self.path)
        if not entries or entries[0]['kind'] != 'prepared' or entries[0]['plan_sha256'] != digest(self.plan):
            raise ValueError('campaign manifest changed')
        current = validate_plan(json.loads((self.root / 'plan.json').read_text()))
        if current != self.plan or entries[0]['code'] != code_identity():
            raise ValueError('pinned campaign code or plan changed')
        return entries

    def append(self, kind, **data):
        return ledger_append(self.path, {'kind': kind, **data})

    def find(self, kind, **values):
        return next((e for e in reversed(self.verify()) if e['kind'] == kind
                     and all(e.get(k) == v for k, v in values.items())), None)

    def totals(self):
        starts = {e['key']: e for e in self.verify() if e['kind'] == 'start'}
        ends = {e['key']: e for e in self.verify() if e['kind'] == 'finish'}
        result = {phase: {'known': 0, 'reserved': 0} for phase in PHASES}
        for key, start in starts.items():
            row = result[start['phase']]
            end = ends.get(key)
            if end is None or end['tokens'] is None:
                row['reserved'] += max(start['cap'], end.get('lower_bound', 0) if end else 0)
            else:
                row['known'] += end['tokens']
        for row in result.values():
            row['committed'] = row['known'] + row['reserved']
        return result

    def call(self, key, phase, cap, callback, **metadata):
        """Reserve before invoking. Interrupted calls never retry or disappear on restart."""
        if self.find('halt'):
            raise ValueError('campaign halted; preserve evidence and prepare a new protocol')
        prior = self.find('finish', key=key)
        if prior:
            start = self.find('start', key=key)
            if (not start or start['phase'] != phase or start['cap'] != cap
                    or start['metadata'] != metadata or prior['phase'] != phase or prior['metadata'] != metadata):
                raise ValueError('cached attempt differs from its pinned request')
            _, tokens = usage_of(prior['result'])
            if tokens != prior['tokens']:
                raise ValueError('cached accounting differs from its result')
            return self.accept_result(key, prior['result'], tokens, cap)
        if self.find('start', key=key):
            raise ValueError(f'interrupted attempt {key}: its full cap remains reserved; no automatic retry')
        if self.totals()[phase]['committed'] + cap + self.plan['margin'] > self.plan['budgets'][phase]:
            raise ValueError(f'{phase} admission budget exhausted (including stop margin)')
        self.append('start', key=key, phase=phase, cap=cap, metadata=metadata)
        started = time.monotonic()
        try:
            result = callback()
            usage, tokens = usage_of(result)
            # Freeze a JSON value, not a reference a callback can mutate after ledgering.
            result = json.loads(json.dumps(result, allow_nan=False))
        except Exception as exc:
            self.append('halt', reason=f'incomplete or invalid attempt {key}: {type(exc).__name__}')
            raise
        self.append('finish', key=key, phase=phase, result=result, tokens=tokens,
                    lower_bound=sum((usage or {}).values()), elapsed_s=time.monotonic()-started, metadata=metadata)
        return self.accept_result(key, result, tokens, cap)

    def accept_result(self, key, result, tokens, cap):
        # Apply this on both fresh and resumed finishes. A crash between finish and halt must
        # never turn an unknown count, overshoot or failed delivery into an accepted observation.
        if tokens is None or tokens > cap or result.get('outcome') != 'done':
            self.append('halt', reason=f'unknown usage, overshoot or delivery failure at {key}')
            raise ValueError('usage/delivery gate failed; accounting is retained')
        return result

    def evaluation(self, evaluator, candidate, task, rep, phase, label, split='dev'):
        actual = next((s for s, tasks in self.plan['splits'].items() if task in tasks), None)
        if actual is None or actual != split:
            raise ValueError('task access differs from its frozen split')
        controls = self.find('controls')
        searched = self.find('search_complete')
        valid = (phase == 'controls' and split == 'dev' and not controls
            or phase == 'search' and split == 'dev' and controls and controls['passed'] and not searched
            or phase == 'manual' and split == 'dev' and searched and not self.find('manual')
            or phase == 'confirmation' and searched and (
                split == 'validation' and not self.find('validation') and not self.find('holdout')
                or split == 'holdout' and self.find('validation') and self.find('validation')['passed']
                   and not self.find('holdout')))
        if not valid or self.find('complete') or self.find('halt'):
            raise ValueError('evaluation is blocked by the campaign stage gate')
        key = f'{label}/{task}/{rep}'
        result = self.call(key, phase, self.plan['run_cap'],
            lambda: evaluator(copy.deepcopy(candidate), task, rep, self.plan['run_cap'], self.plan['seconds']),
            task=task, rep=rep, policy=digest(candidate), label=label, split=split)
        if type(result.get('passed')) is not bool or result.get('protected_pass') is not True:
            self.append('halt', reason=f'protected quality gate failed at {key}')
            raise ValueError('invalid outcome or protected regression')
        return {'task': task, 'rep': rep, 'passed': result['passed'], 'tokens': usage_of(result)[1]}

    def score(self, rows):
        groups = {}
        for row in rows:
            groups.setdefault(self.plan['clusters'][row['task']], []).append(int(row['passed']))
        return statistics.mean(statistics.mean(v) for v in groups.values())

    def controls(self, evaluator):
        with CampaignLock(self.root):
            done = self.find('controls')
            if done:
                return done
            if self.find('halt'):
                raise ValueError('campaign halted')
            schedule = [(t, rep) for t in self.plan['splits']['dev']
                        for rep in range(1, self.plan['repetitions']+1)]
            rng = random.Random(self.plan['seed'])
            rng.shuffle(schedule)
            rows = {'baseline': [], 'degraded': []}
            for task, rep in schedule:
                arms = list(rows)
                rng.shuffle(arms)
                for arm in arms:
                    rows[arm].append(self.evaluation(evaluator, self.plan[arm], task, rep,
                                                    'controls', f'controls-{arm}'))
            paired = []
            for cluster in sorted({self.plan['clusters'][t] for t in self.plan['splits']['dev']}):
                means = {arm: statistics.mean(int(r['passed']) for r in runs
                         if self.plan['clusters'][r['task']] == cluster) for arm, runs in rows.items()}
                paired.append(means['baseline']-means['degraded'])
            # One-sided cluster bootstrap: tiny panels remain exploratory even if this passes.
            draws = sorted(statistics.mean(rng.choices(paired, k=len(paired))) for _ in range(2000))
            lower = draws[99]
            passed = (len(paired) >= 3 and self.score(rows['baseline']) >= self.plan['min_baseline']
                      and lower >= self.plan['min_sensitivity'])
            return self.append('controls', passed=passed, baseline=self.score(rows['baseline']),
                               degraded=self.score(rows['degraded']), lower=lower, clusters=len(paired))

    def dev_history(self):
        """Never expose prepared manifest or sealed results to a proposer."""
        return copy.deepcopy([e for e in self.verify() if e['kind'] in ('proposal', 'decision')
            or e['kind'] == 'finish' and e['metadata'].get('split') == 'dev'
                and ('task' not in e['metadata'] or e['metadata']['task'] in self.plan['splits']['dev'])])

    def promote(self, candidate_rows, incumbent_rows, best_quality):
        quality, incumbent = self.score(candidate_rows), self.score(incumbent_rows)
        cost = sum(r['tokens'] for r in candidate_rows)
        old = sum(r['tokens'] for r in incumbent_rows)
        if quality < best_quality-self.plan['quality_tolerance']:
            return False
        gain = quality >= incumbent+self.plan['min_gain'] and cost <= old*(1+self.plan['cost_allowance'])
        savings = old > 0 and quality >= incumbent-self.plan['quality_tolerance'] and cost <= old*(1-self.plan['min_savings'])
        return gain or savings

    def search(self, evaluator, proposer):
        with CampaignLock(self.root):
            if self.find('search_complete'):
                return self.find('search_complete')
            control = self.find('controls')
            if not control or not control['passed']:
                raise ValueError('passing sensitivity controls are required before search')
            if self.find('validation') or self.find('holdout') or self.find('halt'):
                raise ValueError('search is closed')
            incumbent, stalls, best_quality = self.plan['baseline'], 0, control['baseline']
            for round_no in range(1, self.plan['rounds']+1):
                previous = self.find('decision', round=round_no)
                if previous:
                    incumbent, stalls, best_quality = previous['incumbent'], previous['stalls'], previous['best_quality']
                    if stalls >= 2:
                        break
                    continue
                parent = copy.deepcopy(incumbent)
                # Both siblings see the same parent and the same pre-round development history.
                opened = self.find('round_opened', round=round_no)
                if not opened:
                    opened = self.append('round_opened', round=round_no, parent=parent, history=self.dev_history())
                if opened['parent'] != parent:
                    raise ValueError('round parent differs from its pinned incumbent')
                history = opened['history']
                candidates = []
                for sibling in (1, 2):
                    key = f'proposal/{round_no}/{sibling}'
                    result = self.call(key, 'search', self.plan['proposal_cap'],
                        lambda s=sibling: proposer(copy.deepcopy(parent), copy.deepcopy(history), round_no, s,
                                                   self.plan['proposal_cap']), round=round_no, split='dev')
                    try:
                        candidate = policy(result['policy'])
                        changes = [k for k in POLICY_DEFAULTS if candidate[k] != parent[k]]
                        if len(changes) != 1 or changes[0] not in self.plan['coverage']:
                            raise ValueError('one covered mechanism must change')
                        if not isinstance(result.get('hypothesis'), str) or not result['hypothesis'].strip():
                            raise ValueError('proposal needs a hypothesis')
                        candidates.append((sibling, candidate))
                        reason = 'eligible'
                    except (ValueError, KeyError) as exc:
                        candidate, reason = None, str(exc)
                    if not self.find('proposal', round=round_no, sibling=sibling):
                        self.append('proposal', round=round_no, sibling=sibling, parent=digest(parent),
                                    parent_policy=parent, policy=candidate,
                                    diff={k: [parent[k], candidate[k]] for k in POLICY_DEFAULTS
                                          if candidate is not None and candidate[k] != parent[k]},
                                    hypothesis=result.get('hypothesis'), screen=reason)
                schedule = [(t, rep) for t in self.plan['splits']['dev']
                            for rep in range(1, self.plan['search_repetitions']+1)]
                random.Random(self.plan['seed']+round_no).shuffle(schedule)
                results = {0: [], **{s: [] for s, _ in candidates}}
                policies = {0: parent, **dict(candidates)}
                for task, rep in schedule:
                    # Interleave all policies for contemporary comparisons.
                    arms = list(policies)
                    random.Random(f'{round_no}/{task}/{rep}').shuffle(arms)
                    for arm in arms:
                        results[arm].append(self.evaluation(evaluator, policies[arm], task, rep,
                            'search', f'round-{round_no}-arm-{arm}'))
                acceptable = [s for s, _ in candidates if self.promote(results[s], results[0], best_quality)]
                winner = min(acceptable, key=lambda s: (-self.score(results[s]), sum(r['tokens'] for r in results[s]), s)) if acceptable else 0
                incumbent = policies[winner]
                best_quality = max(best_quality, self.score(results[winner]))
                stalls = 0 if winner else stalls+1
                self.append('decision', round=round_no, parent=digest(parent), winner=winner,
                            incumbent=incumbent, best_quality=best_quality, stalls=stalls)
                if stalls >= 2:
                    break
            return self.append('search_complete', incumbent=incumbent, original=self.plan['baseline'],
                               adoption=False, certification=False)

    def confirm(self, evaluator):
        with CampaignLock(self.root):
            complete = self.find('complete')
            if complete:
                return complete
            searched = self.find('search_complete')
            if not searched or self.find('halt'):
                raise ValueError('completed search is required')
            policies = {'baseline': self.plan['baseline'], 'search': searched['incumbent'], 'manual': self.plan['manual']}
            # Manual development evaluation has the same ceiling as automatic search in a matched
            # protocol. The fixed policy is preregistered; its result never returns to the proposer.
            if not self.find('manual'):
                rows = [self.evaluation(evaluator, policies['manual'], t, r, 'manual', 'manual-dev')
                        for t in self.plan['splits']['dev'] for r in range(1, self.plan['search_repetitions']+1)]
                self.append('manual', score=self.score(rows))
            for split in ('validation', 'holdout'):
                result = self.find(split)
                if result is None:
                    rows = {arm: [] for arm in policies}
                    schedule = [(t, rep) for t in self.plan['splits'][split]
                                for rep in range(1, self.plan['search_repetitions']+1)]
                    random.Random(f'{self.plan["seed"]}/{split}').shuffle(schedule)
                    for task, rep in schedule:
                        for arm, selected in policies.items():
                            rows[arm].append(self.evaluation(evaluator, selected, task, rep,
                                'confirmation', f'{split}-{arm}', split=split))
                    passed = self.promote(rows['search'], rows['baseline'], self.score(rows['baseline']))
                    automatic, manual = rows['search'], rows['manual']
                    result = self.append(split, passed=passed, scores={a: self.score(v) for a, v in rows.items()},
                        tokens={a: sum(r['tokens'] for r in v) for a, v in rows.items()},
                        search_vs_manual={'quality_delta': self.score(automatic)-self.score(manual),
                            'token_delta': sum(r['tokens'] for r in automatic)-sum(r['tokens'] for r in manual),
                            'promotion_rule_passed': self.promote(automatic, manual, self.score(manual)),
                            'equal_admission_ceilings': True,
                            'actual_arm_spending': {k: self.totals()[k]['committed'] for k in ('search', 'manual')}},
                        policies={a: digest(p) for a, p in policies.items()})
                if not result['passed']:
                    return self.append('complete', passed=False, stopped_at=split, adoption=False, certification=False)
            return self.append('complete', passed=True, adoption=False, certification=False,
                               acceptance='original-baseline gate only; see held-out manual comparison',
                               search_vs_manual=self.find('holdout')['search_vs_manual'],
                               scope='exploratory owner-specific evaluation profile')

    def status(self):
        entries = self.verify()
        return {'stage': entries[-1]['kind'], 'live': False, 'blockers': BLOCKERS,
                'totals': self.totals(), 'adoption': False, 'certification': False}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('prepare', help='freeze a private protocol; does not make provider calls')
    p.add_argument('--plan', required=True)
    p.add_argument('--campaign', required=True)
    p = sub.add_parser('status')
    p.add_argument('campaign')
    p = sub.add_parser('budget', help='size the full schedule without inference or authorization')
    p.add_argument('--plan', required=True)
    p = sub.add_parser('offline-demo', help='synthetic full-loop exercise; zero provider calls')
    p.add_argument('--campaign', required=True)
    sub.add_parser('live-preflight', help='fail closed until the broker is certified')
    args = parser.parse_args(argv)
    if args.command == 'budget':
        print(json.dumps(admission_schedule(json.loads(Path(args.plan).read_text())), sort_keys=True))
        return 0
    if args.command == 'live-preflight':
        print(json.dumps({'ready': False, 'blockers': BLOCKERS}))
        return 2
    if args.command == 'offline-demo':
        campaign = Campaign.create(args.campaign, demonstration_plan())
        print(json.dumps(offline_demonstration(campaign), sort_keys=True))
        return 0
    if args.command == 'prepare':
        campaign = Campaign.create(args.campaign, json.loads(Path(args.plan).read_text()))
    else:
        campaign = Campaign(args.campaign)
    print(json.dumps(campaign.status(), sort_keys=True))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
