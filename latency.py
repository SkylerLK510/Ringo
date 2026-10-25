"""Offline paired latency assessment. No provider calls or protected-evaluator changes."""
import argparse
import hashlib
import json
import math
from pathlib import Path
import sys


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), allow_nan=False).encode()


def sha(value):
    return hashlib.sha256(canonical(value)).hexdigest()


def read_json(path):
    def pairs(items):
        obj = {}
        for k, v in items:
            if k in obj:
                raise ValueError(f'duplicate JSON key: {k}')
            obj[k] = v
        return obj
    def bad(value):
        raise ValueError(f'invalid JSON constant: {value}')
    return json.loads(Path(path).read_text(encoding='utf-8'), object_pairs_hook=pairs, parse_constant=bad)


def number(value):
    try:
        return type(value) in (int, float) and math.isfinite(value) and value >= 0
    except OverflowError:
        return False


def percentile(values, q):
    ordered = sorted(values)
    index = (len(ordered) - 1) * q
    a = int(index)
    return ordered[a] + (ordered[min(a + 1, len(ordered) - 1)] - ordered[a]) * (index - a)


def measure(a, b):
    avg_a, avg_b = math.fsum(a) / len(a), math.fsum(b) / len(b)
    return [(avg_b - avg_a) * 1000, avg_b / avg_a - 1,
            (percentile(b, .95) - percentile(a, .95)) * 1000,
            percentile(b, .95) / percentile(a, .95) - 1]


def assess(data, policy):
    """Per-workload mean AND p95 must clear absolute AND relative budgets.

    Sampling unit is a whole paired observation. Workloads must not be pooled:
    one report describes one fixed workload/protocol. CI is a local bootstrap
    estimate, not a hardware-independent guarantee or an RRSI superiority test.
    """
    import random
    if not isinstance(policy, dict) or set(policy) != {'absolute_ms', 'relative', 'min_pairs', 'resamples', 'seed'}:
        raise ValueError('policy needs absolute_ms,relative,min_pairs,resamples,seed')
    if not all(number(policy[k]) for k in ('absolute_ms', 'relative')) \
            or type(policy['min_pairs']) is not int or policy['min_pairs'] < 30 \
            or type(policy['resamples']) is not int or not 500 <= policy['resamples'] <= 50_000 \
            or type(policy['seed']) is not int:
        raise ValueError('finite nonnegative budgets, min_pairs >= 30, 500..50000 resamples and integer seed required')
    if not isinstance(data, dict) or set(data) != {'version', 'baseline', 'candidate', 'protocol', 'planned_pairs', 'pairs'} \
            or type(data['version']) is not int or data['version'] != 1:
        raise ValueError('measurement needs version,baseline,candidate,protocol,planned_pairs,pairs')
    for key in ('baseline', 'candidate'):
        if not isinstance(data[key], str) or not data[key].strip():
            raise ValueError('baseline and candidate identities are required')
    protocol = data['protocol']
    if not isinstance(protocol, dict) or set(protocol) != {'workload', 'environment', 'baseline_config',
            'candidate_config', 'order', 'warmups', 'clock', 'kind'} \
            or any(not isinstance(protocol[k], str) or not protocol[k].strip()
                   for k in set(protocol) - {'warmups'}) \
            or type(protocol['warmups']) is not int or protocol['warmups'] < 0 \
            or protocol['order'] not in ('alternating', 'randomized') \
            or protocol['clock'] != 'perf_counter_ns' \
            or protocol['kind'] not in ('simulated', 'live'):
        raise ValueError('declare one workload, environment, configs, order, warmups, perf_counter_ns and kind')
    count = data['planned_pairs']
    if type(count) is not int or not 1 <= count <= 100_000 or not isinstance(data['pairs'], list) \
            or len(data['pairs']) != count:
        raise ValueError('all planned pairs must be present (1..100000); never drop failed attempts')
    seen, a, b, failed_a, failed_b = set(), [], [], 0, 0
    for row in data['pairs']:
        if not isinstance(row, dict) or set(row) != {'id', 'baseline_s', 'candidate_s', 'baseline_ok', 'candidate_ok'} \
                or not isinstance(row['id'], str) or not row['id'] or row['id'] in seen \
                or not all(number(row[k]) and 0 < row[k] <= 86400 for k in ('baseline_s', 'candidate_s')) \
                or any(type(row[k]) is not bool for k in ('baseline_ok', 'candidate_ok')):
            raise ValueError('each pair needs a unique id, finite durations in (0,86400] seconds and boolean outcomes')
        seen.add(row['id'])
        a.append(row['baseline_s'])
        b.append(row['candidate_s'])
        failed_a += not row['baseline_ok']
        failed_b += not row['candidate_ok']
    point = measure(a, b)
    rng, draws = random.Random(policy['seed']), []
    for _ in range(policy['resamples']):
        indices = [rng.randrange(count) for _ in range(count)]
        draws.append(measure([a[i] for i in indices], [b[i] for i in indices]))
    # Bonferroni simultaneous intervals for four comparisons, two-sided 95% family confidence.
    tail = .05 / (2 * 4)
    bounds = [[percentile([d[j] for d in draws], tail),
               percentile([d[j] for d in draws], 1 - tail)] for j in range(4)]
    limits = [policy['absolute_ms'], policy['relative']] * 2
    if failed_b > failed_a:
        status, reason = 'slower', 'candidate adds failed attempts; fast failures cannot pass'
    elif any(interval[0] > limit for interval, limit in zip(bounds, limits)):
        status, reason = 'slower', 'a lower confidence bound exceeds a declared latency budget'
    elif count < policy['min_pairs'] or failed_a or failed_b:
        status, reason = 'uncertain', 'insufficient planned sample or observed failures'
    elif all(interval[1] <= limit for interval, limit in zip(bounds, limits)):
        status, reason = 'negligible', 'all upper confidence bounds fit the declared budgets on this workload'
    else:
        status, reason = 'uncertain', 'confidence bounds overlap a declared budget'
    dist = lambda values: {f'p{q}': percentile(values, q / 100) * 1000 for q in (50, 95, 99)}
    return {'version': 1, 'status': status, 'reason': reason, 'measurement': data, 'policy': policy,
            'measurement_sha256': sha(data), 'policy_sha256': sha(policy),
            'assessor_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            'metrics': dict(zip(('mean_delta_ms', 'mean_relative', 'p95_delta_ms', 'p95_relative'),
                               [{'estimate': v, 'interval': ci} for v, ci in zip(point, bounds)])),
            'baseline_ms': dist(a), 'candidate_ms': dist(b),
            'failures': {'baseline': failed_a, 'candidate': failed_b},
            'limits': 'Self-reported paired measurements; simultaneous bootstrap bounds, one workload. '
                      'Simulated backends do not establish live provider latency. No RRSI quality claim.'}


def verify_report(report):
    if not isinstance(report, dict) or 'measurement' not in report or 'policy' not in report \
            or canonical(assess(report['measurement'], report['policy'])) != canonical(report):
        raise ValueError('latency evidence changed or was assessed by a different implementation')
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--measurements', type=Path, required=True)
    parser.add_argument('--absolute-ms', type=float, required=True)
    parser.add_argument('--relative', type=float, required=True)
    parser.add_argument('--min-pairs', type=int, default=100)
    parser.add_argument('--resamples', type=int, default=2000)
    parser.add_argument('--seed', type=int, default=1)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        import harness
        out = harness.outside_repo(args.out, 'latency report')
        report = assess(read_json(args.measurements), {'absolute_ms': args.absolute_ms,
            'relative': args.relative, 'min_pairs': args.min_pairs, 'resamples': args.resamples, 'seed': args.seed})
        with out.open('x', encoding='utf-8') as f:
            json.dump(report, f, indent=2, allow_nan=False)
            f.write('\n')
        print(report['status'] + ': ' + report['reason'])
        return {'negligible': 0, 'slower': 1, 'uncertain': 3}[report['status']]
    except (ValueError, OSError, OverflowError, RecursionError) as exc:
        print(f'latency: {exc}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
