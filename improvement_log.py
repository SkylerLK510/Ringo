"""Ringo's improvement log, recomputed from committed raw evidence.

    python3 improvement_log.py          # check every entry and print the log
    python3 improvement_log.py --write  # check, then rewrite docs/IMPROVEMENT-LOG.md

Entries live in docs/improvements/log.json, their evidence beside it. Every number in the log is
computed here from that evidence; an entry whose evidence does not show the improvement is refused.
"""
import hashlib
import json
import re
import sys
from datetime import date
from pathlib import Path

HERE = Path(__file__).resolve().parent
LOG = HERE / 'docs/improvements/log.json'
PAGE = HERE / 'docs/IMPROVEMENT-LOG.md'
REPO = 'https://github.com/SkylerLK510/Ringo'
FIELDS = {'date', 'title', 'scope', 'kind', 'change', 'evidence', 'claim'}


def read(path):
    return json.loads(path.read_text(encoding='utf-8'))


def latency_rows(entry, files):
    """(claimed rows, other rows). A claimed workload needs no failed attempt in either arm and a mean-delta
    simultaneous interval wholly below zero; no workload in the same evidence may show a demonstrated slowdown."""
    import latency
    reports = [r for path in files for r in (read(path).get('reports') or [read(path)])]
    rows = {}
    for report in reports:
        r = latency.assess(report['measurement'], report['policy'])  # recomputed from the raw pairs, never trusted
        name = r['measurement']['protocol']['workload']
        if name in rows:  # a second report must not replace the first, which may hold the failed attempts
            raise ValueError(f'workload {name} appears in more than one report')
        m = r['metrics']['mean_delta_ms']
        if m['interval'][0] > 0:
            raise ValueError(f"{name}: demonstrated slowdown in the same evidence {m['interval']}")
        rows[name] = {'workload': name, 'pairs': r['measurement']['planned_pairs'], 'kind': r['measurement']['protocol']['kind'],
                      'before_p50': r['baseline_ms']['p50'], 'after_p50': r['candidate_ms']['p50'],
                      'delta': m['estimate'], 'interval': m['interval'], 'failures': r['failures']}
    for name in entry['claim']:
        row = rows.get(name)
        if row is None:
            raise ValueError(f'claimed workload {name} is not in the evidence')
        if row['failures'] != {'baseline': 0, 'candidate': 0}:
            raise ValueError(f'{name}: failed attempts cannot show an improvement')
        if row['interval'][1] >= 0:
            raise ValueError(f"{name}: the interval {row['interval']} does not exclude no change")
    return [rows[n] for n in entry['claim']], [r for n, r in rows.items() if n not in entry['claim']]


def token_rows(entry, files):
    """Rows for before/after call pairs. Usage is reparsed from each raw event stream; a pair needs byte-identical
    input (same stdin hash), the same model and effort, reported usage on both calls and fewer input tokens after."""
    import harness
    data = read(files[0])
    calls = {}
    for c in data['calls']:
        _, usage = harness.codex_reply('\n'.join(json.dumps(e) for e in c['events']), 'codex-chatgpt', [])
        if not set(harness.TOKEN_KEYS) <= set(usage):
            raise ValueError(f"{c['id']}: no reported usage")
        argv = c['argv']
        model = argv[argv.index('--model') + 1] if '--model' in argv[:-1] else None
        effort = next((a for a in argv if a.startswith('model_reasoning_effort=')), None)
        if not (model and effort):  # a CLI default could differ between the two sides
            raise ValueError(f"{c['id']}: the argv must pin --model and model_reasoning_effort")
        calls[c['id']] = {**c, 'input': usage['input'] + usage['cache_read'] + usage['cache_write'],
                          'output': usage['output'], 'model': model, 'effort': effort}
    rows = []
    for kind, pairs in (('pair', data['pairs']), ('repeat', data.get('repeats', []))):
        for a, b in pairs:
            x, y = calls[a], calls[b]
            if (x['stdin_sha256'], x['model'], x['effort']) != (y['stdin_sha256'], y['model'], y['effort']):
                raise ValueError(f'{a} and {b} differ in input, model or effort')
            if kind == 'pair' and not y['input'] < x['input']:
                raise ValueError(f'{b} does not use fewer input tokens than {a}')
            if kind == 'repeat' and x['input'] != y['input']:
                raise ValueError(f'{a} and {b}: identical input reported different token counts')
            rows.append({'kind': kind, 'step': x['step'], 'before': x, 'after': y})
    if [r['step'] for r in rows if r['kind'] == 'pair'] != entry['claim']:
        raise ValueError('claimed steps must match the evidence pairs')
    when = max(r['after']['measured_at'] for r in rows if r['kind'] == 'pair')[:10]
    if entry['date'] != when:
        raise ValueError(f"date {entry['date']} is not the measurement date {when}")
    return rows


def check(entry):
    if set(entry) != FIELDS or entry['kind'] not in ('latency', 'tokens'):
        raise ValueError(f'an entry needs exactly {sorted(FIELDS)} and kind latency or tokens')
    date.fromisoformat(entry['date'])
    for prose in ('title', 'scope'):  # numbers come from the evidence, never typed into prose
        if re.search(r'\d', entry[prose]):
            raise ValueError(f'{prose} must not contain numbers: {entry[prose]!r}')
    files = []
    for name, digest in entry['evidence'].items():
        path = LOG.parent / name
        if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError(f'{name}: evidence changed after it was logged')
        files.append(path)
    return latency_rows(entry, files) if entry['kind'] == 'latency' else token_rows(entry, files)


def ms(v):
    return f'{v:+.2f}'.replace('-', '−')


def pct(after, before):
    return f'{(after - before) / before * 100:+.1f}%'.replace('-', '−')


def render():
    entries = sorted(read(LOG), key=lambda e: e['date'], reverse=True)  # stable: same-day entries keep log order
    out = ['# Improvement log', '',
           'Generated by `python3 improvement_log.py --write` from `docs/improvements/log.json`; do not edit it by hand.',
           '`tests/test_improvement_log.py` fails when this page differs from what the evidence produces, so every',
           'number here is recomputed from raw evidence committed under `docs/improvements/`.', '',
           'An entry is admitted only when its evidence shows the improvement: latency claims need a paired',
           'simultaneous interval wholly below zero, no failed attempts and no demonstrated slowdown anywhere in the',
           'same measurement; token claims need byte-identical input, the same model and effort, and provider-reported',
           'usage on both sides. Evidence files are hash-pinned. The date is when the improvement was measured: token',
           'evidence carries call timestamps and the date is checked against them; latency reports carry no clock, so',
           'their date is the experimenter\'s record. The commit that added an entry records when it entered the',
           'repository.', '',
           '| Measured | Improvement | Change | Result |', '|---|---|---|---|']
    sections = []
    for e in entries:
        rows = check(e)
        change = ', '.join(f'[#{n}]({REPO}/pull/{n})' for n in e['change'])
        if e['kind'] == 'latency':
            claimed, other = rows
            deltas = sorted(r['delta'] for r in claimed)
            span = ms(deltas[0]) if len(deltas) == 1 else f'{ms(deltas[-1])} to {ms(deltas[0])}'
            summary = f"mean {span} ms on {len(claimed)} workload{'s' * (len(claimed) != 1)}"
            body = ['| Workload | Pairs | Before p50 ms | After p50 ms | Mean change ms (simultaneous interval) |',
                    '|---|---:|---:|---:|---|']
            body += [f"| {r['workload']} | {r['pairs']} | {r['before_p50']:.2f} | {r['after_p50']:.2f} | "
                     f"{ms(r['delta'])} [{ms(r['interval'][0])}, {ms(r['interval'][1])}] |" for r in claimed]
            if other:
                body += ['', 'Measured in the same run but not claimed (interval includes no change, or the arm failed by design): ' +
                         '; '.join(f"{r['workload']} {ms(r['delta'])} [{ms(r['interval'][0])}, {ms(r['interval'][1])}] ms"
                                   for r in other) + '.']
        else:
            pairs = [r for r in rows if r['kind'] == 'pair']
            summary = '; '.join(f"{r['step']} input {r['before']['input']:,} → {r['after']['input']:,} "
                                f"({pct(r['after']['input'], r['before']['input'])})" for r in pairs)
            body = ['| Call | Before input tokens | After input tokens | Change | Output tokens before / after |',
                    '|---|---:|---:|---:|---|']
            body += [f"| {r['step']} | {r['before']['input']:,} | {r['after']['input']:,} | "
                     f"{pct(r['after']['input'], r['before']['input'])} | {r['before']['output']:,} / {r['after']['output']:,} |"
                     for r in pairs]
            body += ['', 'Input tokens include cached input. Output varies between samples and is not claimed.']
            body += [f"Repeat check: the same {r['step']} input sent again reported {r['after']['input']:,} input tokens, "
                     f"the same as the first call." for r in rows if r['kind'] == 'repeat']
        out.append(f"| {e['date']} | {e['title']} | {change} | {summary} |")
        evidence = ', '.join(f'[`{n}`](improvements/{n}) (sha256 `{d[:12]}`)' for n, d in e['evidence'].items())
        sections += ['', f"## {e['date']} · {e['title']}", '', e['scope'], '', *body, '', f'Evidence: {evidence}.']
    return '\n'.join(out + sections) + '\n'


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    page = render()
    if '--write' in argv:
        PAGE.write_text(page, encoding='utf-8')
    else:
        print(page, end='')
    return 0


if __name__ == '__main__':
    sys.exit(main())
