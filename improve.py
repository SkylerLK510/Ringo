"""Ringo improvement loop: bounded self-improvement of the harness's own review pipeline.

The loop follows Meta-Harness (Lee, Nair, Zhang, Lee, Khattab and Finn, 2026, arXiv:2603.28052): an improver reads
the incumbent, dev evidence and previous outcomes and proposes the next pipeline; full traces remain on disk.
Its search discipline comes from
RRSI (Xia et al., 2026, arXiv:2609.24972): one small edit per proposal, a log of rejected hypotheses, a screen for
case-specific content, and a cost allowance that grows only with the measured gain.

The improver may change a review plan (passes, their order, inputs, batch size, per-pass model) and the text of the
prompts the plan uses. It never sees validation or test cases; it cannot edit code, the grading or proposer
prompts, specs, splits or thresholds; and validation gives back only an aggregate decision. Each campaign:

1. runs the incumbent on a seeded sample of dev cases for evidence (what it found, missed and got wrong);
2. asks the proposer, in one isolated call, for one change;
3. screens it in code: one change only, allowed files only, prompt growth capped, and no case id, answer-key
   text, answer-key file path or canary in it (calls per review are not capped: a plan makes the calls its passes
   need, and the cost allowance below prices every one of them, at unpriced_call_usd when none is reported);
4. runs candidate and incumbent on the same dev sample; the candidate advances only if the bootstrap lower bound
   of its gain in U (defects found minus wrong findings) is above zero for every grader, and its cost ratio stays
   within the allowance (base + slope x the mean U gain);
5. submits an advancing candidate to validation (the store caps submissions across campaigns sharing cases);
   a pass makes it the campaign's finalist and ends the campaign.

A finalist is not adopted here. Adopting it takes a sealed-test experiment (evaluate.py freeze on the test split),
which needs graders from two provider families, spends alpha, retires the test cases, and must pass every gate; even
then a person adopts it, since every decision is advisory.

Campaign defaults (JSON; name, incumbent and a conservative unpriced_call_usd must be supplied).
With the default codex grader, allow_tool_backends must also explicitly be true, acknowledging file-access risk:
  {"name": "c1", "incumbent": "plans/review.json", "backend": "claude", "proposer": "claude",
   "graders": ["claude", "codex"],
   "max_proposals": 10, "dev_sample": 12, "dev_runs": 1, "validation_runs": 2, "max_prompt_growth": 1.25,
   "allowance": [1.1, 0.4], "budget_usd": 50, "min_groups": 10,
   "margins": {"U": 0.5, "W": 0.25}, "bootstrap": 2000, "seed": 1, "unpriced_call_usd": null,
   "allow_tool_backends": false, "timeout_s": 900, "grade_timeout_s": 1800, "proposer_timeout_s": 900}
The null price and false tool acknowledgment intentionally refuse these defaults before spending; set them for
your backend costs and isolation. Dev evidence is reused for search, not confirmation. New prompts needed by a
plan edit count with that edit. Validation feedback contains only the decision word.

usage: python3 improve.py --store S --campaign CAMPAIGN.json --workspace DIR
"""
import argparse
import json
import math
import re
import subprocess
import sys
import unicodedata
from pathlib import Path

import backends
import evaluate
import harness
import pipelines

HERE = Path(__file__).resolve().parent
CAMPAIGN_DEFAULTS = {'backend': 'claude', 'proposer': 'claude', 'graders': ['claude', 'codex'], 'max_proposals': 10, 'dev_sample': 12,
                     'dev_runs': 1, 'validation_runs': 2, 'max_prompt_growth': 1.25,
                     'allowance': [1.1, 0.4], 'budget_usd': 50, 'min_groups': 10, 'margins': {'U': 0.5, 'W': 0.25},
                     'bootstrap': 2000, 'seed': 1, 'unpriced_call_usd': None, 'allow_tool_backends': False,
                     'timeout_s': harness.TIMEOUT_S, 'grade_timeout_s': 1800, 'proposer_timeout_s': harness.TIMEOUT_S}
CONTROL_PROMPTS = {'grade.md', 'improve.md'}
NGRAM = 8  # a proposal may not share any run of this many words with a dev answer key


def load_campaign(path):
    camp = {**CAMPAIGN_DEFAULTS, **json.loads(Path(path).read_text(encoding='utf-8'))}
    if 'max_calls_per_review' in camp:
        raise ValueError('max_calls_per_review was removed: each plan makes the calls its passes need')
    unknown = set(camp) - set(CAMPAIGN_DEFAULTS) - {'name', 'incumbent'}
    if unknown or not isinstance(camp.get('name'), str) or not camp.get('incumbent'):
        raise ValueError(f'a campaign needs name and incumbent; unknown keys: {sorted(unknown)}')
    if not re.fullmatch(pipelines.ID, camp['name']):
        raise ValueError('campaign name must be a safe id')
    for key in ('max_proposals', 'dev_sample', 'dev_runs', 'validation_runs', 'bootstrap'):
        if type(camp[key]) is not int or camp[key] < 1:
            raise ValueError(f'{key} must be a positive integer')
    if type(camp['min_groups']) is not int or camp['min_groups'] < evaluate.SPEC_DEFAULTS['min_groups']:
        raise ValueError('min_groups must be an integer >= 10')
    if type(camp['seed']) is not int:
        raise ValueError('seed must be an integer')
    if not isinstance(camp['margins'], dict) or set(camp['margins']) != {'U', 'W'} or any(
            harness.valid_price(x) is None for x in camp['margins'].values()):
        raise ValueError('margins must contain finite, non-negative U and W values')
    if harness.valid_price(camp['budget_usd']) is None:
        raise ValueError('budget_usd must be finite and non-negative')
    if harness.valid_price(camp['max_prompt_growth']) is None or camp['max_prompt_growth'] < 1:
        raise ValueError('max_prompt_growth must be finite and >= 1')
    if not isinstance(camp['allowance'], list) or len(camp['allowance']) != 2 \
            or any(harness.valid_price(x) is None for x in camp['allowance']):
        raise ValueError('allowance must be two finite, non-negative numbers')
    if harness.valid_price(camp['unpriced_call_usd']) is None:
        raise ValueError('set unpriced_call_usd in the campaign: every backend can have an attempt with no reported cost')
    if type(camp['allow_tool_backends']) is not bool:
        raise ValueError('allow_tool_backends must be a bool')
    harness.check_backends([camp['backend'], camp['proposer'], *camp['graders']], camp['allow_tool_backends'])
    for key in ('timeout_s', 'grade_timeout_s', 'proposer_timeout_s'):
        if harness.valid_price(camp[key]) is None or camp[key] <= 0:
            raise ValueError(f'{key} must be finite and positive')
    camp['incumbent'] = str((Path(path).resolve().parent / camp['incumbent']).resolve())
    return camp


def redact_paths(text, paths):
    for path in sorted(paths, key=len, reverse=True):
        # Also strip partial paths ending in the answer-key basename.
        text = re.sub(r'(?<![\w.-])(?:[\w.-]+/)*' + re.escape(Path(path).name) + r'(?![\w.-])', '[file]', text)
    return text


def evidence(store, d, arm):
    """Dev-only evidence for the proposer from a graded dev experiment: per case, what the arm's reviews found,
    which known defects they missed, and which findings were judged wrong (first grader)."""
    store = Path(store)
    spec = evaluate.checked(store, d)
    if spec['split'] != 'dev':
        raise evaluate.EvalError('evidence comes from dev experiments only')
    key = json.loads((store / 'keys' / f'{d}.json').read_text(encoding='utf-8'))
    gdir = store / 'runs' / d / 'grades' / evaluate.sha(spec['graders'][0])[:12]
    out = []
    for cid in evaluate.split_cases(store, 'dev', spec):
        path = gdir / f'{cid}.json'
        if not path.exists():
            continue
        entries = {e['id']: e for e in evaluate.gold_entries(store, cid)}
        paths = {e['path'] for e in entries.values() if e['path']}
        for rid, gr in json.loads(path.read_text(encoding='utf-8')).items():
            if key[rid].split('/')[1].split('.')[0] != arm:
                continue
            missed = [entries[g] for g in entries if g not in gr['found']]
            out.append(f'--- dev case {len(out) + 1}: found {len(gr["found"])} of {len(entries)} known defects; '
                       f'{len(gr["invalid"])} wrong findings\n'
                       + ''.join(f"missed ({e['note'] or ''}): {redact_paths(e['message'], paths)}\n" for e in missed)
                       + ''.join(f'judged wrong: {redact_paths(w, paths)}\n' for w in gr['invalid']))
    return '\n'.join(out)


def plan_files(plan_path):
    return bare(pipelines.load_plan(plan_path, 'review'))


def bare(plan):
    """Normalized plan data and its used prompt texts."""
    texts = {p['prompt']: p['text'] for p in plan['passes']}
    bare = {k: v for k, v in plan.items() if k != 'prompts_dir'}
    bare['passes'] = [{k: v for k, v in p.items() if k != 'text'} for p in plan['passes']]
    return bare, texts


def parse_proposal(text):
    """The one top-level JSON proposal in a reply (backends.top_level_objects); ValueError if none or several."""
    matches = [o for o in backends.top_level_objects(text) if 'hypothesis' in o and not o.repeats_a_key]
    if len(matches) != 1 or not isinstance(matches[0]['hypothesis'], str):
        raise ValueError(f'{len(matches)} JSON proposals with a hypothesis; need exactly one')
    found = matches[0]
    if 'plan' in found and not isinstance(found['plan'], dict):
        raise ValueError('plan must be an object')
    if 'prompts' in found and (not isinstance(found['prompts'], dict)
                               or not all(isinstance(k, str) and isinstance(v, str) for k, v in found['prompts'].items())):
        raise ValueError('prompts must map file names to text')
    return found


def strings(obj):
    """Every string in a JSON value, including object keys, recursively."""
    if isinstance(obj, str):
        yield obj
    elif isinstance(obj, dict):
        for k, v in obj.items():
            yield from strings(k)
            yield from strings(v)
    elif isinstance(obj, list):
        for v in obj:
            yield from strings(v)


def words(text):
    """Lower-case words with whitespace, punctuation and invisible format characters (zero-width spaces and the
    like) removed, so wrapping, reflowing or hidden characters cannot disguise copied text."""
    text = ''.join(ch for ch in unicodedata.normalize('NFKC', text) if unicodedata.category(ch) != 'Cf')
    return re.findall(r'\w+', text.lower())


def candidate_parts(incumbent, proposal):
    plan, texts = plan_files(incumbent)
    new_plan = proposal.get('plan', plan)
    used = {p['prompt'] for p in new_plan['passes']}
    library = {p.name: p.read_text(encoding='utf-8').strip() for p in pipelines.PROMPTS.glob('*.md')
               if p.name not in CONTROL_PROMPTS and p.name in used - set(texts)}
    return plan, texts, new_plan, {**library, **texts, **proposal.get('prompts', {})}


def candidate_key(incumbent, proposal):
    _, _, plan, texts = candidate_parts(incumbent, proposal)
    with backends.private_tempdir(prefix='ringo-screen-') as d:
        for n, t in texts.items():
            (Path(d) / n).write_text(t, encoding='utf-8')
        normalized, used = bare(pipelines.validate(json.loads(json.dumps(plan)), d))
    return evaluate.sha(json.dumps([normalized, used], sort_keys=True))


def screen(store, proposal, incumbent, dev_ids, camp):
    """Screen actual normalized instructions, never treating heuristics as an isolation guarantee."""
    why = contamination_reasons(store, '\n'.join(strings(proposal)), dev_ids)
    supplied = proposal.get('prompts', {})
    if any(not re.fullmatch(r'[\w.-]+\.md', n) for n in supplied):
        return ['a prompt file name must be a plain .md name']
    try:
        plan, texts, raw, new_texts = candidate_parts(incumbent, proposal)
        if raw.get('kind') != 'review':
            return ['the improver may only propose a review plan']
        with backends.private_tempdir(prefix='ringo-screen-') as d:
            for n, t in new_texts.items():
                (Path(d) / n).write_text(t, encoding='utf-8')
            new_plan, normalized = bare(pipelines.validate(json.loads(json.dumps(raw)), d))
    except (ValueError, TypeError, KeyError, AttributeError, OSError) as exc:
        return [f'the plan is invalid or malformed: {exc}']
    used, introduced = set(normalized), set(normalized) - set(texts)
    changed = [n for n in supplied if texts.get(n) != supplied[n].strip()]
    if set(changed) - used:
        why.append('it changes a prompt its plan does not use')
    old_changes = sum(texts.get(n) != t for n, t in normalized.items() if n not in introduced)
    if (new_plan != plan) + old_changes != 1:
        why.append('a proposal makes exactly one change: one prompt file, or the plan with its new prompts')
    if any(p['reply'] == 'verdicts' and any(q['reply'] == 'findings' and q['prompt'] == p['prompt']
           for q in new_plan['passes']) for p in new_plan['passes']):
        why.append('a checker needs a different prompt from a reader')
    before = len(plan['role']) + sum(map(len, texts.values()))
    after = len(new_plan['role']) + sum(len(t) for n, t in normalized.items() if n not in introduced)
    growth = camp['max_prompt_growth']
    if after > before * growth:
        why.append(f'its instructions grow {after / max(1, before):.2f}x (limit {growth}x)')
    for n in introduced:
        if len(new_plan['role']) + len(normalized[n]) > growth * (len(plan['role']) + max(map(len, texts.values()))):
            why.append('a new prompt exceeds the per-call growth cap')
    return why


def contamination_reasons(store, text, dev_ids):
    """Stop on any hidden-key match before giving ordinary rejection feedback."""
    why = []
    flat = ' '.join(words(text))
    if evaluate.leaked(store, text) or evaluate.leaked(store, flat.replace(' ', '')):
        raise evaluate.EvalError('answer-key contamination detected; stopping campaign')
    mine = words(text)
    grams = {' '.join(mine[k:k + NGRAM]) for k in range(len(mine) - NGRAM + 1)}
    # ceiling: Dev is deliberately adaptive. Hidden-key matching is never returned as proposer feedback.
    pool = evaluate.cases(store)
    basename_groups = {}
    for cid, meta in pool.items():
        for entry in evaluate.gold_entries(store, cid):
            if entry['path']:
                basename_groups.setdefault(Path(entry['path']).name, set()).add(meta['group'])
    dev_matches, hidden_matches = set(), set()
    for cid in pool:
        matches = set()
        if f' {" ".join(words(cid))} ' in f' {flat} ':
            matches.add(('case', cid))
        for entry in evaluate.gold_entries(store, cid):
            w = words(entry['message'])
            matches.update(('text', gram) for k in range(len(w) - NGRAM + 1)
                           if (gram := ' '.join(w[k:k + NGRAM])) in grams)
            # Short descriptions must be checked as a complete phrase, not an eight-word window.
            if 1 < len(w) < NGRAM:
                if f' {" ".join(w)} ' in f' {flat} ':
                    matches.add(('text', ' '.join(w)))
            if entry['path']:
                names = [entry['path']]
                base = Path(entry['path']).name
                if len(basename_groups[base]) == 1:
                    names.append(base)
                for name in names:
                    normalized = ' '.join(words(name))
                    if f' {normalized} ' in f' {flat} ':
                        matches.add(('path', normalized))
        if cid in dev_ids:
            dev_matches.update(matches)
            if matches:
                why.append('it repeats a dev case id or answer-key text' if any(k != 'path' for k, _ in matches)
                           else 'it names a file from a dev answer key')
        else:
            hidden_matches.update(matches)
    # Shared dev phrases are already exposed; they still reject a proposal, but are not evidence of a hidden leak.
    if hidden_matches - dev_matches:
        raise evaluate.EvalError('answer-key contamination detected; stopping campaign')
    return list(dict.fromkeys(why))


def build_candidate(ws, n, incumbent, proposal):
    _, _, plan, texts = candidate_parts(incumbent, proposal)
    d = Path(ws) / 'candidates' / str(n)
    (d / 'prompts').mkdir(parents=True, exist_ok=True)
    for name, text in texts.items():
        (d / 'prompts' / name).write_text(text + '\n', encoding='utf-8')
    (d / 'plan.json').write_text(json.dumps({**proposal.get('plan', plan), 'prompts_dir': 'prompts'}, indent=1),
                                 encoding='utf-8')
    (d / 'proposal.json').write_text(json.dumps(proposal, indent=1), encoding='utf-8')
    return d / 'plan.json'


def experiment(store, ws, name, arms, split, camp, comparators=(), runs=1, sample=None, budget=None,
               call_fn=harness.call, digests=None, grading_reserve=0):
    spec = {'name': name, 'campaign': camp['name'], 'split': split, 'arms': arms, 'candidate': next(iter(arms)),
            'budget_usd': budget, 'unpriced_call_usd': camp['unpriced_call_usd'],
            'allow_tool_backends': camp['allow_tool_backends'], 'grading_reserve_usd': grading_reserve,
            'timeout_s': camp['timeout_s'], 'grade_timeout_s': camp['grade_timeout_s'],
            'comparators': list(comparators), 'graders': camp['graders'], 'runs': runs, 'min_groups': camp['min_groups'],
            'margins': camp['margins'], 'bootstrap': camp['bootstrap'], 'seed': camp['seed'], 'sample': sample}
    path = Path(ws) / 'specs' / f'{name}.json'
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(spec, indent=1), encoding='utf-8')
    d = evaluate.freeze(store, path)
    if digests is not None:
        digests.append(d)  # register spending before any call, even if running or grading is interrupted
    evaluate.run(store, d, call_fn=call_fn, stop_on_failure=bool(comparators))
    evaluate.grade(store, d, call_fn=call_fn)
    return d, evaluate.score(store, d)


def spent(store, digests):
    """USD spent by a campaign's experiments, rebuilt from disk (evaluate.cost); inf if any of it is unknown."""
    total = 0.0
    for d in digests:
        usd, unknown = evaluate.cost(store, d)
        if unknown:
            return float('inf')
        total += usd
    return total


def proposer_cost(ws):
    """Proposer spending from its charge log; an interrupted or unpriced attempt makes it unknown."""
    path = Path(ws) / 'proposer-charges.jsonl'
    attempts = {}
    if path.exists():
        for line in path.read_text(encoding='utf-8').splitlines():
            charge = json.loads(line)
            attempts[charge['n']] = harness.valid_price(charge.get('cost_usd')) if charge['status'] == 'settled' else None
    return float('inf') if any(x is None for x in attempts.values()) else sum(attempts.values())


def campaign(store, campaign_path, ws, call_fn=harness.call):
    """Run one campaign; returns its log. Every step is also in the store's ledger."""
    store, ws = evaluate.outside_repo(store, 'store'), evaluate.outside_repo(ws, 'workspace')
    camp = load_campaign(campaign_path)
    for name in [camp['backend'], camp['proposer'], *camp['graders']]:
        evaluate.identity(name, store)
    evaluate.check_policy({'split': 'validation', 'objective': 'quality', 'margins': camp['margins'],
                           'max_cost_ratio': evaluate.SPEC_DEFAULTS['max_cost_ratio'], 'bootstrap': camp['bootstrap']},
                          evaluate.policy(store), evaluate.alpha(store, 'validation'))
    pool = evaluate.cases(store)
    for part, selection in [('dev', {'sample': camp['dev_sample'], 'seed': camp['seed']}), ('validation', None)]:
        ids = evaluate.split_cases(store, part, selection)
        if len({pool[c]['group'] for c in ids}) < camp['min_groups']:
            raise ValueError(f'{part} needs at least {camp["min_groups"]} repository groups before a campaign can spend')
    # The campaign workspace is caller-selected and may have a shared parent.
    backends.private_directory(ws, create=True, parents=True, exist_ok=True)
    with evaluate.exclusive(ws / '.campaign.lock', 'campaign'):
        with evaluate.locked(store):
            if any(r['kind'] in ('campaign_start', 'campaign') and r['name'] == camp['name']
                   for r in evaluate.ledger(store)):
                raise evaluate.EvalError('this campaign already started; its budget cannot be reset')
            if any(p.name != '.campaign.lock' for p in ws.iterdir()):
                raise evaluate.EvalError('a campaign workspace must be new or empty')
            evaluate.record(store, 'campaign_start', name=camp['name'], budget_usd=camp['budget_usd'], workspace=str(ws))
        return _campaign(store, camp, ws, call_fn)


def estimated_experiment(store, previous, n_cases, n_arms, runs):
    """Pilot-based planning estimate, not a provider-side cap or a promise about a changed pipeline."""
    spec = evaluate.prereg(store, previous)['spec']
    old_slots = evaluate.slots(store, spec)
    root = Path(store) / 'runs' / previous / 'out'
    costs = [evaluate.slot_cost(root / c / f'{a}.{r}', spec['unpriced_call_usd']) for c, a, r in old_slots]
    grades = [c['cost_usd'] for c in evaluate.charges(store, previous)]
    if not old_slots or any(x is None for x in costs + grades):
        return math.inf, math.inf
    scale = n_cases * n_arms * runs / len(old_slots)
    # One largest observed call supplies overshoot headroom; grading scales with the number of reviews.
    calls = [harness.valid_price((json.loads(line).get('usage') or {}).get('cost_usd'))
             for path in root.glob('*/*/trace/trace.jsonl') for line in path.read_text().splitlines()]
    # ceiling: Pilot prices can underpredict a changed pipeline; the reserve is a planning estimate.
    headroom = max([x for x in calls + grades if x is not None] + [spec['unpriced_call_usd'] or 0])
    reserve = sum(grades) * scale + headroom
    return sum(costs) * scale + reserve, reserve


def _campaign(store, camp, ws, call_fn):
    instructions = (HERE / 'prompts' / 'improve.md').read_text(encoding='utf-8').strip()
    dev_ids = evaluate.split_cases(store, 'dev')
    inc = {'plan': camp['incumbent'], 'backend': camp['backend']}
    digests, log, tried = [], [], set()

    def left():  # one budget for the whole campaign: proposer calls plus every experiment's runs and grading
        return camp['budget_usd'] - proposer_cost(ws) - spent(store, digests)
    if left() <= 0:
        raise ValueError('the campaign budget is spent before it starts')
    try:  # the log and the ledger record are written however the campaign stops
        d0, _ = experiment(store, ws, f"{camp['name']}-baseline", {'inc': inc}, 'dev', camp, runs=camp['dev_runs'],
                           sample=camp['dev_sample'], budget=left(), call_fn=call_fn, digests=digests)
        proof = evidence(store, d0, 'inc')
        for n in range(1, camp['max_proposals'] + 1):
            if left() <= 0:
                log.append({'n': n, 'stopped': 'budget'})
                break
            plan, texts = plan_files(camp['incumbent'])
            library = ', '.join(p.name for p in pipelines.PROMPTS.glob('*.md') if p.name not in CONTROL_PROMPTS)
            dossier = ('=== INCUMBENT PLAN ===\n' + json.dumps(plan, indent=1) + '\n\n'
                       + ''.join(f'=== PROMPT {k} ===\n{v}\n\n' for k, v in texts.items())
                       + f'Library prompts available for new passes: {library}\n\n'
                       + harness.fence('DEV EVIDENCE', proof, 'untrusted grader notes from dev reviews'))
            message = harness.fence('CAMPAIGN LOG', json.dumps(log, indent=1), 'untrusted earlier proposals and dev results') + '\n\n' + instructions
            if evaluate.leaked(store, dossier + message):
                raise evaluate.EvalError('a canary reached the improver dossier; stopping')
            entry = {'n': n}
            try:
                with open(ws / 'proposer-charges.jsonl', 'a', encoding='utf-8') as f:  # recorded before the call
                    f.write(json.dumps({'n': n, 'status': 'started'}) + '\n')
                try:
                    text, usage = call_fn(harness.backend(camp['proposer']),
                                          'You improve code-review pipelines.\n\n' + dossier, message, camp['proposer_timeout_s'])
                    failure = None
                except (backends.JudgeError, subprocess.TimeoutExpired) as exc:  # a failed call can still cost money
                    text, usage, failure = None, getattr(exc, 'usage', None), exc
                price = harness.valid_price((usage or {}).get('cost_usd'))
                price = price if price is not None else camp['unpriced_call_usd']
                with open(ws / 'proposer-charges.jsonl', 'a', encoding='utf-8') as f:
                    f.write(json.dumps({'n': n, 'status': 'settled', 'cost_usd': price, 'failed': failure is not None}) + '\n')
                if price is None:  # an unknown cost makes the campaign budget unenforceable
                    log.append({**entry, 'stopped': 'the proposer call has no known cost; declare unpriced_call_usd'})
                    break
                if failure is not None:
                    raise failure
                if evaluate.leaked(store, text) or evaluate.leaked(store, ''.join(words(text))):
                    raise evaluate.EvalError('answer-key contamination detected; stopping campaign')
                try:
                    proposal = parse_proposal(text)
                except ValueError:
                    contamination_reasons(store, text, dev_ids)
                    raise
            except (ValueError, backends.JudgeError, subprocess.TimeoutExpired) as exc:
                log.append({**entry, 'result': 'rejected', 'why': [f'no usable proposal: {exc}']})
                continue
            entry['hypothesis'] = proposal['hypothesis']
            if left() <= 0:
                log.append({**entry, 'stopped': 'budget'})
                break
            why = screen(store, proposal, camp['incumbent'], dev_ids, camp)
            if why:
                log.append({**entry, 'result': 'rejected', 'why': why})
                continue
            key = candidate_key(camp['incumbent'], proposal)
            if key in tried:
                log.append({**entry, 'result': 'rejected', 'why': ['it repeats an earlier candidate of this campaign']})
                continue
            tried.add(key)
            estimate, reserve = estimated_experiment(store, digests[-1], len(evaluate.split_cases(store, 'dev')[:camp['dev_sample']]), 2, camp['dev_runs'])
            if left() < estimate:
                log.append({**entry, 'stopped': 'budget', 'needed': estimate, 'left': max(0, left())})
                break
            cand = {'plan': str(build_candidate(ws, n, camp['incumbent'], proposal)), 'backend': camp['backend']}
            d, rep = experiment(store, ws, f"{camp['name']}-dev-{n}", {'cand': cand, 'inc': inc}, 'dev', camp,
                                comparators=['inc'], runs=camp['dev_runs'], sample=camp['dev_sample'], budget=left(),
                                call_fn=call_fn, digests=digests, grading_reserve=reserve)
            if rep['integrity_errors'] or rep['cost_unknown'] or rep['failed'] or rep['completed_slots'] != rep['expected_slots'] \
                    or any(g['missing_grades'] for g in rep['graders'].values()) or rep['groups'] < camp['min_groups']:
                log.append({**entry, 'result': 'rejected', 'why': ['the dev run is incomplete, inconsistent, unpriced or has too few repository groups']})
                continue
            gains = [g['vs']['inc']['U'] for g in rep['graders'].values() if g['vs'].get('inc', {}).get('U')]
            mean_gain = sum(x['mean'] for x in gains) / len(gains) if gains else 0.0
            c, b = rep['cost_per_review'].get('cand'), rep['cost_per_review'].get('inc')
            limit = camp['allowance'][0] + camp['allowance'][1] * max(0.0, mean_gain)
            ratio = evaluate.cost_ratio(c, b)
            dev = {'U_gain': round(mean_gain, 3), 'U_lower': [round(x['boot_lower'], 3) for x in gains],
                   'cost_ratio': None if ratio is None else round(ratio, 3) if math.isfinite(ratio) else 'inf', 'allowance': round(limit, 3)}
            if len(gains) != len(camp['graders']) or not all(x['boot_lower'] > 0 for x in gains) or ratio is None or ratio > limit:
                log.append({**entry, 'result': 'rejected', 'why': ['no clear dev gain within the cost allowance'], 'dev': dev})
                continue
            try:
                estimate, reserve = estimated_experiment(store, d, len(evaluate.split_cases(store, 'validation')), 2, camp['validation_runs'])
                if left() < estimate:
                    log.append({**entry, 'stopped': 'budget', 'dev': dev, 'needed': estimate, 'left': max(0, left())})
                    break
                dv, _ = experiment(store, ws, f"{camp['name']}-val-{n}", {'cand': cand, 'inc': inc}, 'validation', camp,
                                   comparators=['inc'], runs=camp['validation_runs'], budget=left(), call_fn=call_fn,
                                   digests=digests, grading_reserve=reserve)
            except evaluate.EvalError:  # no evaluator details return to the proposer
                log.append({**entry, 'result': 'stopped', 'why': ['validation unavailable'], 'dev': dev})
                break
            result, _ = evaluate.decide(store, dv)
            log.append({**entry, 'result': f'validation {result}', 'dev': dev, 'candidate': cand['plan']})
            if result == 'pass':
                break
    finally:
        total = proposer_cost(ws) + spent(store, digests)
        log.append({'cost_usd': round(total, 4) if math.isfinite(total) else None, 'cost_unknown': not math.isfinite(total)})
        (ws / 'log.json').write_text(json.dumps(log, indent=1), encoding='utf-8')
        evaluate.record(store, 'campaign', name=camp['name'], proposals=sum('hypothesis' in e for e in log),
                        finalist=next((e['candidate'] for e in log if e.get('result') == 'validation pass'), None))
    return log


def main(argv=None):
    p = argparse.ArgumentParser(prog='improve.py', description='Run one bounded improvement campaign.')
    p.add_argument('--store', required=True)
    p.add_argument('--campaign', required=True)
    p.add_argument('--workspace', required=True)
    args = p.parse_args(argv)
    try:
        log = campaign(args.store, args.campaign, args.workspace)
    except (evaluate.EvalError, ValueError, OSError) as exc:
        print(f'improve: {exc}', file=sys.stderr)
        return 2
    print(json.dumps(log, indent=1))
    return 0


if __name__ == '__main__':
    sys.exit(main())
