"""Shadow labels for `work` runs: what each finished run already says about stopping after round 1. No model calls.

One row per run folder (it holds report.json and spec.json):
- regime: a hash of the harness configuration: the exact implementation, writer prompt, artifact kind, backends,
  roles and loop settings, never the task. Rows from different regimes are never pooled; a model, prompt or release
  change starts a new regime (docs/AGCL.md, "declared drift").
- eligible: round 1 passed every check, so a policy that stops on passing checks could have stopped there.
- identical: the delivered editable files are byte-identical to round 1's, so stopping would have delivered the same
  files. A run whose files never change cannot be harmed by stopping, so the change rate bounds the harm rate.
- tokens in round 1 and after it, from the trace.
- with grade.json beside report.json ({"round1_pass": bool, "delivered_pass": bool}, written by an evaluation with
  hidden tests): harm, meaning round 1 failed the hidden tests and the delivered files passed them.

The summary gives, per regime, exact one-sided upper bounds (Clopper-Pearson) on the change rate among the eligible
runs and on the harm rate among eligible graded runs. Every run here ran the full baseline: the stop rule these labels
were collected for was measured and rejected (docs/AGCL.md), so no run skips its review.

usage: python3 shadow_labels.py RUN_OR_FOLDER... [--jsonl OUT] [--alpha 0.05]
"""
import argparse
import ast
import hashlib
import json
import math
import sys
from pathlib import Path

import workflow


def upper_bound(k, n, alpha=0.05):
    """Exact one-sided (Clopper-Pearson) upper confidence bound on a rate with k events in n trials."""
    if n == 0:
        return 1.0
    if k >= n:
        return 1.0

    def cdf(p):  # P(X <= k) for X ~ Binomial(n, p), in logs so large n stays stable
        if p <= 0:
            return 1.0
        if p >= 1:
            return 0.0
        return sum(math.exp(math.lgamma(n + 1) - math.lgamma(i + 1) - math.lgamma(n - i + 1)
                            + i * math.log(p) + (n - i) * math.log1p(-p)) for i in range(k + 1))
    lo, hi = k / n, 1.0
    for _ in range(100):
        mid = (lo + hi) / 2
        lo, hi = (mid, hi) if cdf(mid) > alpha else (lo, mid)
    return hi


def usage_events(trace):
    if not trace.is_file():
        return
    for line in trace.read_text(encoding='utf-8').splitlines():
        event = json.loads(line)
        if event.get('status') == 'ok' and event.get('usage'):
            usage = event['usage'] if isinstance(event['usage'], dict) else ast.literal_eval(event['usage'])
            yield event['step'], usage


def kind(report, spec):
    return report.get('artifact_kind') or workflow.artifact_kind(spec)  # older reports did not record it


def regime(report, spec):
    """The harness configuration a run used, never its task: tasks are the samples a regime's bounds count."""
    key = {'implementation': (report.get('implementation') or {}).get('id'), 'kind': kind(report, spec),
           'writer_prompt': report.get('writer_prompt_sha256'),
           **{k: spec.get(k) for k in ('writer', 'judge', 'roles', 'protocol', 'protocol_repair', 'no_regression',
                                       'max_rounds', 'candidates')},
           **{k: k in spec for k in ('spec_tests', 'scripts', 'council')}}
    return hashlib.sha256(json.dumps(key, sort_keys=True).encode()).hexdigest()[:12]


def row(run):
    report = json.loads((run / 'report.json').read_text(encoding='utf-8'))
    spec = json.loads((run / 'spec.json').read_text(encoding='utf-8'))
    rounds = report.get('rounds') or []
    first = rounds[0] if rounds else {}
    eligible = bool(first.get('checks')) and all(c['status'] == 'pass' for c in first['checks'])
    names = sorted(spec['files'])
    read = lambda folder, name: (folder / name).read_bytes() if (folder / name).is_file() else None
    delivered = run / 'final' if report.get('final') else None
    identical = None
    if eligible and delivered and (run / 'round-1/candidate').is_dir():
        identical = all(read(run / 'round-1/candidate', n) == read(delivered, n) for n in names)
    first_tokens = after_tokens = 0
    for step, usage in usage_events(run / 'trace.jsonl'):
        size = sum(usage.get(k) or 0 for k in ('input', 'cache_read', 'cache_write', 'output'))
        if step == 'spec-tests' or step == 'writer-1' or step.startswith('writer-1.'):
            first_tokens += size
        else:
            after_tokens += size
    out = {'run': str(run), 'regime': regime(report, spec), 'implementation': (report.get('implementation') or {}).get('id'),
           'artifact_kind': kind(report, spec), 'status': report.get('status'), 'rounds': len(rounds),
           'eligible': eligible, 'identical': identical,
           'round1_tokens': first_tokens, 'after_tokens': after_tokens}
    if (run / 'grade.json').is_file():
        grade = json.loads((run / 'grade.json').read_text(encoding='utf-8'))
        out.update(round1_pass=grade['round1_pass'], delivered_pass=grade['delivered_pass'], suspect=grade.get('suspect', False),
                   harm=bool(eligible and not grade['round1_pass'] and grade['delivered_pass']),
                   avoided=bool(eligible and grade['round1_pass'] and not grade['delivered_pass']))
    return out


def runs(paths):
    for path in paths:
        path = Path(path)
        found = [path] if (path / 'report.json').is_file() else sorted(p.parent for p in path.rglob('report.json'))
        yield from (p for p in found if (p / 'spec.json').is_file())


def summary(rows, alpha):
    lines = []
    for key in sorted({r['regime'] for r in rows}):
        rs = [r for r in rows if r['regime'] == key]
        eligible = [r for r in rs if r['eligible'] and not r.get('suspect')]  # a flagged run counts toward no bound
        compared = [r for r in eligible if r['identical'] is not None]
        changed = sum(not r['identical'] for r in compared)
        graded = [r for r in eligible if 'harm' in r]
        harms = sum(r['harm'] for r in graded)
        suffix = [r['after_tokens'] for r in eligible]
        total = sum(r['round1_tokens'] + r['after_tokens'] for r in rs)
        lines.append(f"regime {key} ({(rs[0]['implementation'] or '?')[:21]}, {rs[0]['artifact_kind']}): {len(rs)} runs, "
                     f"{len(eligible)} eligible")
        lines.append(f'  change rate after round 1: {changed}/{len(compared)}, upper bound '
                     f'{upper_bound(changed, len(compared), alpha):.1%} (bounds the harm of stopping)')
        suspect = sum(bool(r.get('suspect')) for r in rs)
        if suspect:
            lines.append(f'  {suspect} graded runs left out: a draft touched files or processes (see grade.json)')
        if graded:
            lines.append(f'  graded harm: {harms}/{len(graded)}, upper bound {upper_bound(harms, len(graded), alpha):.1%}; '
                         f"stopping would have avoided {sum(r['avoided'] for r in graded)} regressions")
        if suffix and total:
            lines.append(f'  tokens after round 1 on eligible runs: mean {sum(suffix) / len(suffix):,.0f}; '
                         f"{sum(r['after_tokens'] for r in eligible) / total:.0%} of this regime's tokens")
    lines.append(f'(one-sided {1 - alpha:.0%} upper bounds; rows from different regimes are not pooled)')
    return '\n'.join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('paths', nargs='+', help='run folders, or folders that contain them')
    parser.add_argument('--jsonl', type=Path, help='also write one row per run here')
    parser.add_argument('--alpha', type=float, default=0.05)
    args = parser.parse_args(argv)
    rows = [row(run) for run in runs(args.paths)]
    if not rows:
        print('shadow labels: no work run folders found', file=sys.stderr)
        return 1
    if args.jsonl:
        args.jsonl.write_text(''.join(json.dumps(r) + '\n' for r in rows), encoding='utf-8')
    print(summary(rows, args.alpha))
    return 0


if __name__ == '__main__':
    sys.exit(main())
