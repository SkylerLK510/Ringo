"""HarBenchmark v0.1: public development cases and an offline, harness-neutral scorer.

No inference, optimization, network activity, or changes to Ringo's runtime path.
See HarBenchmark.md for the submission contract and deliberately limited claims.
"""
import argparse
import ast
import hashlib
import json
import math
from pathlib import Path
import sys

VERSION = '0.1'
HERE = Path(__file__).resolve().parent


def case(cid, track, task, shape, expected=None, syntax=False):
    return dict(id=cid, track=track, task=task, answer_shape=shape,
                expected=expected, syntax=syntax)


CASES = [
    case('review-fallback', 'review',
         'Review this new fallback. The API must preserve stored zero balances.\n'
         'balance.py:1 def balance(data, default=100):\n'
         'balance.py:2     return data.get("balance") or default\n'
         'Report the number of defects and a minimal counterexample with actual and required results.',
         {'defects': 'integer', 'input': 'object', 'actual': 'number', 'required': 'number'},
         {'defects': 1, 'input': {'balance': 0}, 'actual': 100, 'required': 0}),
    case('review-clean', 'review',
         'Review this implementation against its complete contract: preserve zero, return 100 only when '
         'the key is absent. Values are integers.\n'
         'def balance(data):\n    return data.get("balance", 100)\n'
         'Report only the number of demonstrated contract violations.',
         {'defects': 'integer'}, {'defects': 0}),
    case('repair-zero', 'coding',
         'Repair the expression data.get("balance") or default, where zero is a valid stored '
         'balance and the default is used only for an absent key. Use a single data.get call '
         'with two positional arguments (the key and default). Return only the replacement expression '
         'as a JSON string in replacement. This task checks the specified expression form.',
         {'replacement': 'Python expression string'},
         {'replacement': 'data.get("balance", default)'}, syntax=True),
    case('repair-none', 'coding',
         'Repair the condition in if not value: where the fallback must run only for None, '
         'and 0, False and the empty string must survive unchanged. Use Python identity comparison '
         'of value with None (value on the left). Return only the condition as a JSON string '
         'in replacement, without if or colon. This task checks the specified expression form.',
         {'replacement': 'Python expression string'}, {'replacement': 'value is None'}, syntax=True),
    case('design-capacity', 'design',
         'Choose the cheapest feasible worker count from 1 through 10. Demand is 240 jobs/s; '
         'one worker sustains 80 jobs/s; utilization must be at most 0.60 even after one worker fails. '
         'Each worker costs $20/hour. Return worker count, utilization after one failure and hourly cost.',
         {'workers': 'integer', 'failure_utilization': 'number', 'hourly_usd': 'number'},
         {'workers': 6, 'failure_utilization': 0.6, 'hourly_usd': 120}),
    case('design-quorum', 'design',
         'A five-replica store requires read/write and write/write quorum intersection '
         '(R+W>5 and 2W>5). Writes must tolerate two unavailable replicas. '
         'Choose the smallest feasible write quorum, then the smallest read quorum. '
         'This is quorum arithmetic only; do not infer full linearizability.',
         {'read_quorum': 'integer', 'write_quorum': 'integer'},
         {'read_quorum': 3, 'write_quorum': 3}),
    case('artifact-release', 'artifact',
         'Produce structured release-note data from this authoritative inventory: '
         'release 2.4.0, added streaming export (I-12), fixed empty-file crash (I-18), '
         'removed legacy XML import (I-22). Keep categories and issue ids exact; '
         'do not invent features. Return version and issue-id arrays added/fixed/removed.',
         {'version': 'string', 'added': 'array of ids', 'fixed': 'array of ids', 'removed': 'array of ids'},
         {'version': '2.4.0', 'added': ['I-12'], 'fixed': ['I-18'], 'removed': ['I-22']}),
    case('artifact-table', 'artifact',
         'Create structured data for a report table: alpha has 8 passes and 2 failures; beta has '
         '3 passes and 2 failures. Return rows in alpha,beta order, with name, total, pass_rate '
         '(fraction), and a total row named all. Aggregate counts, not rates.',
         {'rows': 'array of objects with name,total,pass_rate'},
         {'rows': [{'name': 'alpha', 'total': 10, 'pass_rate': 0.8},
                   {'name': 'beta', 'total': 5, 'pass_rate': 0.6},
                   {'name': 'all', 'total': 15, 'pass_rate': 11 / 15}]}),
    case('research-uncertainty', 'research',
         'For independent observations [2,4,6,8], compute mean, unbiased sample variance '
         '(ddof=1), and standard error of the mean. Return numerical results, without rounding '
         'below seven significant digits. These are synthetic observations.',
         {'mean': 'number', 'sample_variance': 'number', 'standard_error': 'number'},
         {'mean': 5, 'sample_variance': 20 / 3, 'standard_error': math.sqrt(5 / 3)}),
    case('research-drawdown', 'research',
         'A synthetic portfolio starts at 100 and has successive simple returns +0.10,-0.20,+0.25. '
         'No fees or deposits. Return final wealth, compounded total return and maximum drawdown '
         'as a positive fraction of the running peak (including initial wealth).',
         {'final_wealth': 'number', 'total_return': 'number', 'max_drawdown': 'number'},
         {'final_wealth': 110, 'total_return': 0.10, 'max_drawdown': 0.20}),
    case('evaluation-macro', 'evaluation',
         'Two completed reviews found 1 of 1 gold defect and 0 of 9 gold defects. '
         'Return macro recall, micro recall, and whether this exposed public-development sample '
         'alone justifies adopting a new default pipeline.',
         {'macro_recall': 'number', 'micro_recall': 'number', 'adopt_default': 'boolean'},
         {'macro_recall': 0.5, 'micro_recall': 0.1, 'adopt_default': False}),
    case('evaluation-accounting', 'evaluation',
         'A work task has max_rounds=2 and samples=6, one writer and six judge calls per '
         'well-formed new candidate. Return maximum calls. One actual attempt timed out without '
         'price or token usage, and no fallback price was declared. Return cost_usd as null, '
         'cost_known as false and whether it is valid to report that attempt as zero-cost.',
         {'maximum_calls': 'integer', 'cost_usd': 'null', 'cost_known': 'boolean', 'report_zero_cost': 'boolean'},
         {'maximum_calls': 14, 'cost_usd': None, 'cost_known': False, 'report_zero_cost': False}),
]

def strict_json(text):
    def pairs(items):
        out = {}
        for key, value in items:
            if key in out:
                raise ValueError(f'duplicate JSON key: {key}')
            out[key] = value
        return out

    def constant(value):
        raise ValueError(f'nonfinite JSON number: {value}')

    value = json.loads(text, object_pairs_hook=pairs, parse_constant=constant)
    def finite(item):
        if isinstance(item, float) and not math.isfinite(item):
            raise ValueError('nonfinite JSON number')
        if isinstance(item, dict):
            for v in item.values():
                finite(v)
        if isinstance(item, list):
            for v in item:
                finite(v)
    finite(value)
    return value


def suite_hash():
    # Covers cases AND scoring implementation. Any source edit => different suite.
    return hashlib.sha256(Path(__file__).read_bytes()).hexdigest()


def selected(ids=None):
    ids = [c['id'] for c in CASES] if ids is None else ids
    if not ids or len(ids) != len(set(ids)) or set(ids) - {c['id'] for c in CASES}:
        raise ValueError('select nonempty, unique, known case ids')
    return [next(c for c in CASES if c['id'] == cid) for cid in ids]


def equal(got, want):
    if type(want) in (int, float):
        if type(got) not in (int, float):
            return False
        try:
            return math.isfinite(got) and math.isclose(got, want, rel_tol=1e-6, abs_tol=1e-9)
        except OverflowError:
            return False
    if isinstance(want, dict):
        return isinstance(got, dict) and got.keys() == want.keys() and all(equal(got[k], v) for k, v in want.items())
    if isinstance(want, list):
        return isinstance(got, list) and len(got) == len(want) and all(equal(a, b) for a, b in zip(got, want))
    return type(got) is type(want) and got == want


def syntax_equal(answer, expected):
    if not isinstance(answer, dict) or set(answer) != {'replacement'} \
            or not isinstance(answer['replacement'], str) or len(answer['replacement']) > 32_000:
        return False
    try:
        tree = lambda text: ast.dump(ast.parse(text.strip(), mode='eval'))
        return tree(answer['replacement']) == tree(expected['replacement'])
    except (SyntaxError, ValueError, RecursionError):
        return False


def score(submission, manifest):
    # Schedule comes from the export, never from the answers that happened to arrive.
    if not isinstance(manifest, dict) or manifest.get('suite_sha256') != suite_hash() \
            or manifest.get('version') != VERSION or not isinstance(manifest.get('cases'), list):
        raise ValueError('manifest does not match this benchmark; re-export')
    if any(not isinstance(c, dict) or not isinstance(c.get('id'), str) for c in manifest['cases']):
        raise ValueError('manifest cases need string ids')
    items = selected([c['id'] for c in manifest['cases']])
    if manifest != export([c['id'] for c in items]):
        raise ValueError('manifest was changed; use the original pre-generation export')
    if not isinstance(submission, dict) or set(submission) != {'version', 'suite_sha256', 'metadata', 'case_ids', 'answers'}:
        raise ValueError('submission needs exactly version,suite_sha256,metadata,case_ids,answers')
    if submission['version'] != VERSION or submission['suite_sha256'] != suite_hash():
        raise ValueError('suite version/hash mismatch; re-export after changing the benchmark')
    meta = submission['metadata']
    if not isinstance(meta, dict) or set(meta) != {'harness', 'model', 'revision', 'run_kind', 'evidence'} \
            or any(not isinstance(v, str) or not v.strip() for v in meta.values()) \
            or meta['run_kind'] not in ('model_run', 'fixture'):
        raise ValueError('metadata needs harness,model,revision,run_kind (model_run/fixture),evidence strings')
    if not isinstance(submission['case_ids'], list) or any(not isinstance(x, str) for x in submission['case_ids']):
        raise ValueError('case_ids must be an array of strings')
    if submission['case_ids'] != [c['id'] for c in items]:
        raise ValueError('submission case_ids must match the pre-generation export schedule')
    answers = submission['answers']
    if not isinstance(answers, dict) or set(answers) - set(submission['case_ids']):
        raise ValueError('answers must be an object containing only selected case ids')
    results, tracks = [], {}
    for item in items:
        cid = item['id']
        status, error = 'missing', 'no answer supplied'
        if cid in answers:
            record = answers[cid]
            if not isinstance(record, dict) or record.get('status') not in ('ok', 'failed', 'unsupported'):
                status, error = 'malformed', 'record needs status ok/failed/unsupported'
            elif record['status'] != 'ok':
                status, error = record['status'], 'harness reported ' + record['status']
            elif set(record) != {'status', 'answer'}:
                status, error = 'malformed', 'ok record needs exactly status and answer'
            else:
                check = syntax_equal if item['syntax'] else equal
                answer = record['answer']
                integer_fields = [k for k, shape in item['answer_shape'].items() if shape == 'integer']
                integers_valid = isinstance(answer, dict) and all(type(answer.get(k)) is int for k in integer_fields)
                status = 'passed' if integers_valid and check(answer, item['expected']) else 'failed'
                error = '' if status == 'passed' else 'answer violates the case contract'
        results.append({'id': cid, 'track': item['track'], 'status': status, 'error': error})
        row = tracks.setdefault(item['track'], {'selected': 0, 'passed': 0,
                                               'failed': 0, 'missing': 0, 'malformed': 0, 'unsupported': 0})
        row['selected'] += 1
        row[status] += 1
    for row in tracks.values():
        row['pass_rate'] = row['passed'] / row['selected']
    return {'benchmark': 'HarBenchmark', 'version': VERSION, 'suite_sha256': suite_hash(),
            'split': 'public-dev', 'metadata': meta, 'repetitions': 1,
            'coverage': {'selected': len(items), 'available': len(CASES), 'complete': len(items) == len(CASES)},
            'tracks': tracks, 'cases': results,
            'claim': 'public development constraint checks only; provenance is self-reported; no overall ranking'}


def export(ids=None):
    items = selected(ids)
    return {'benchmark': 'HarBenchmark', 'version': VERSION, 'suite_sha256': suite_hash(),
            'split': 'public-dev',
            'cases': [{k: c[k] for k in ('id', 'track', 'task', 'answer_shape')} for c in items],
            'submission_template': {'version': VERSION, 'suite_sha256': suite_hash(),
                'metadata': {'harness': 'REQUIRED', 'model': 'REQUIRED', 'revision': 'REQUIRED',
                             'run_kind': 'model_run', 'evidence': 'REQUIRED: private trace path'},
                'case_ids': [c['id'] for c in items], 'answers': {}}}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    sub.add_parser('list')
    exp = sub.add_parser('export')
    exp.add_argument('--case', action='append', dest='ids')
    exp.add_argument('--out', type=Path, required=True)
    scoring = sub.add_parser('score')
    scoring.add_argument('--manifest', type=Path, required=True)
    scoring.add_argument('--submission', type=Path, required=True)
    scoring.add_argument('--out', type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == 'list':
            for c in CASES:
                print(f"{c['id']}\t{c['track']}")
            return 0
        out = args.out.resolve()
        if out == HERE or HERE in out.parents:
            raise ValueError('keep exported submissions and results outside the repository')
        if args.command == 'score' and args.submission.stat().st_size > 1_048_576:
            raise ValueError('submission exceeds 1 MiB')
        value = export(args.ids) if args.command == 'export' else score(
            strict_json(args.submission.read_text(encoding='utf-8')),
            strict_json(args.manifest.read_text(encoding='utf-8')))
        # Do not replace an earlier report or accidentally overwrite the submission.
        with out.open('x', encoding='utf-8') as f:
            json.dump(value, f, indent=2, allow_nan=False)
            f.write('\n')
        if args.command == 'score':
            print(json.dumps(value['tracks'], sort_keys=True))
            return 0 if all(c['status'] == 'passed' for c in value['cases']) else 1
        return 0
    except (ValueError, OSError, TypeError, OverflowError, RecursionError) as exc:
        print(f'HarBenchmark: {exc}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
