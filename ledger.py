"""Offline ledger: what Ringo's usage record says each agent, surface and model used, with the weights that make
tokens comparable (W) and the fixed/carry/growth split of the context each model call read. Reads files, calls no
model, changes nothing. See docs/TOKEN-USAGE.md "Cost-weighted usage (W) and the ledger".

    python3 ledger.py report    [--state DIR | --usage FILE] [--window W]... [--now EPOCH] [--no-traces] [--trace DIR]... [--json]
    python3 ledger.py attribute [--shell DIR] [--agent codex|claude] [--base N] [--json]
"""
import argparse
import glob
import json
import os
import sys
import time
from pathlib import Path

import harness

COUNTS = ('input', 'cache_read', 'cache_write', 'output')
CTX_KEYS = ('fixed', 'carry', 'growth')
OLD_SURFACES = {'codex': 'cli:codex', 'claude': 'cli:claude', 'agy': 'cli:agy', 'ringo': 'pipeline'}
DEFAULT_WINDOWS = ('24h', '7d', 'all')


class Usage(Exception):
    """A bad argument or an unreadable file: exit 2."""


def state_root():
    return Path(os.environ.get('RINGO_RELEASE_HOME') or Path.home() / '.local/share/ringo')


def whole(x):
    return type(x) is int and x >= 0


def window_seconds(text):
    """'24h' -> 86400, '7d' -> 604800, 'all' -> None."""
    if text == 'all':
        return None
    unit = {'h': 3600, 'd': 86400}.get(text[-1:])
    if unit is None or not text[:-1].isdigit() or int(text[:-1]) == 0:
        raise Usage(f'a window is Nh, Nd or all, not {text!r}')
    return int(text[:-1]) * unit


def counts_of(source):
    if isinstance(source, dict) and all(whole(source.get(k)) for k in COUNTS):
        return {k: source[k] for k in COUNTS}
    return None


def entry(at, agent, surface, model, calls, counts, job, unknown=False, ctx=None, inferred=False):
    return {'at': at, 'agent': agent, 'surface': surface, 'model': model, 'calls': calls, 'counts': counts,
            'unknown': unknown, 'ctx': ctx, 'job': job, 'surface_inferred': inferred}


def from_row(row):
    """One usage.jsonl row as an entry. Rows written before the ledger have no surface (taken from the agent's name)
    and no calls (unknown, never zero)."""
    agent = row.get('agent') if isinstance(row.get('agent'), str) else '?'
    surface = row.get('surface')
    inferred = not isinstance(surface, str)
    if inferred:
        surface = OLD_SURFACES.get(agent, 'unknown')
    unknown = row.get('unknown') is True
    ctx = None
    if all(whole(row.get(k)) for k in ('ctx_base', 'ctx_fixed', 'ctx_carry', 'ctx_growth')):
        ctx = {'base': row['ctx_base'], 'fixed': row['ctx_fixed'], 'carry': row['ctx_carry'], 'growth': row['ctx_growth']}
    return entry(row['at'] if whole(row.get('at')) else None, agent, surface,
                 row.get('model') if isinstance(row.get('model'), str) else None,
                 row['calls'] if whole(row.get('calls')) else None,
                 None if unknown else counts_of(row), row.get('job') if isinstance(row.get('job'), str) else None,
                 unknown, ctx, inferred)


def trace_entries(path, at=None, job=None):
    """One entry per paid attempt of a pipeline's trace.jsonl; None when it cannot be read."""
    try:
        lines = Path(path).read_text(encoding='utf-8', errors='replace').splitlines()
    except OSError:
        return None
    found = []
    for line in lines:
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        if not isinstance(rec, dict) or rec.get('status') not in harness.PAID_STATUSES:
            continue
        usage = rec.get('usage') if isinstance(rec.get('usage'), dict) else None
        surface = (usage or {}).get('surface')
        if not isinstance(surface, str):
            surface = harness.surface(rec['backend']) if isinstance(rec.get('backend'), str) else 'unknown'
        when = at if at is not None else (rec.get('started') if isinstance(rec.get('started'), (int, float)) else None)
        found.append(entry(int(when) if when is not None else None, 'ringo', surface,
                           rec.get('model') if isinstance(rec.get('model'), str) else None, 1, counts_of(usage),
                           job or str(path)))
    return found


def read_entries(usage_file, expand=True, extra_traces=()):
    """(entries, source facts). A line that is not a JSON object is skipped and counted."""
    try:
        text = Path(usage_file).read_text(encoding='utf-8', errors='replace')
    except OSError as e:
        raise Usage(f'cannot read {usage_file}: {e.strerror or e}')
    entries, skipped, rows, expanded = [], 0, 0, 0
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except ValueError:
            row = None
        if not isinstance(row, dict):
            skipped += 1
            continue
        rows += 1
        got = None
        if expand and row.get('agent') == 'ringo' and isinstance(row.get('job'), str):
            got = trace_entries(Path(row['job']) / 'trace' / 'trace.jsonl', row['at'] if whole(row.get('at')) else None, row['job'])
            expanded += got is not None
        entries.extend(got if got is not None else [from_row(row)])
    for given in extra_traces:
        folder = Path(given)
        files = [folder / 'trace.jsonl'] if (folder / 'trace.jsonl').is_file() else sorted(folder.glob('*/trace.jsonl'))
        for f in files:
            entries.extend(trace_entries(f) or [])
            expanded += 1
    return entries, {'path': str(usage_file), 'rows': rows, 'skipped_lines': skipped, 'traces_expanded': expanded}


def shares(parts):
    total = sum(parts.values())
    return {k: (round(v / total, 4) if total else None) for k, v in parts.items()}


def group_of(items):
    agent, surface, model = items[0]['agent'], items[0]['surface'], items[0]['model']
    known = [e for e in items if e['calls'] is not None]
    sums = {k: sum(e['counts'][k] for e in items if e['counts']) for k in COUNTS}
    units, unit, unknown_models, complete = 0, None, set(), True
    for e in items:
        u, n, unknown = harness.weigh(surface, model, e['counts'])
        unit = u
        if unknown:
            unknown_models.add(unknown)
        if u is not None:
            complete = complete and n is not None
            units += n or 0
    if unit is None:
        w = {'unit': None, 'value': None, 'unknown_models': []}
    else:
        value = None
        if complete and units < harness.WEIGHT_LIMIT:
            value = round(float(units) / harness.WEIGHT_UNITS[unit], 6)
        w = {'unit': unit, 'value': value, 'unknown_models': sorted(unknown_models)}
    withctx = [e['ctx'] for e in items if e['ctx']]
    parts = {k: sum(c[k] for c in withctx) for k in CTX_KEYS}
    return {'agent': agent, 'surface': surface, 'model': model,
            'jobs': len({e['job'] if e['job'] is not None else id(e) for e in items}),
            'calls': sum(e['calls'] for e in known), 'calls_unknown_jobs': len(items) - len(known),
            'unknown_jobs': sum(1 for e in items if e['unknown']), **sums, 'T': sum(sums.values()), 'W': w,
            'ctx': {'jobs_with_ctx': len(withctx), **parts, 'shares': shares(parts),
                    'bases': sorted({c['base'] for c in withctx})}}


def windows_of(entries, windows, now):
    out = []
    for text in windows:
        seconds = window_seconds(text)
        kept = [e for e in entries if seconds is None or (e['at'] is not None and now - e['at'] <= seconds)]
        by = {}
        for e in kept:
            by.setdefault((e['agent'], e['surface'], e['model'] or ''), []).append(e)
        out.append({'window': text, 'since': None if seconds is None else now - seconds,
                    'groups': [group_of(by[k]) for k in sorted(by)]})
    return out


def pct(x):
    return '-' if x is None else f'{x * 100:.1f}'


def table(rows):
    widths = [max(len(str(r[i])) for r in rows) for i in range(len(rows[0]))]
    return '\n'.join('  '.join(str(c).ljust(w) if i < 3 else str(c).rjust(w) for i, (c, w) in enumerate(zip(r, widths))).rstrip()
                     for r in rows)


def report_text(doc):
    src = doc['source']
    lines = [f"ledger: {src['path']}: {src['rows']} rows, {src['skipped_lines']} lines skipped, "
             f"{src['traces_expanded']} traces expanded; 0 model calls"]
    if src['skipped_lines']:
        lines.append(f"  {src['skipped_lines']} unreadable rows (not JSON objects; left out of every total)")
    head = ['agent', 'surface', 'model', 'jobs', 'calls', 'input', 'cache_read', 'cache_write', 'output', 'T', 'W',
            'fixed%', 'carry%', 'growth%', 'ctx jobs']
    for w in doc['windows']:
        lines += ['', f"window {w['window']}"]
        if not w['groups']:
            lines.append('  (no rows)')
            continue
        rows = [head]
        for g in w['groups']:
            W = g['W']
            shown = 'n/a' if W['unit'] is None else ('?' if W['value'] is None else f"{W['value']:,} {W['unit']}")
            sh = g['ctx']['shares']
            calls = f"{g['calls']:,}" + (f"+{g['calls_unknown_jobs']}?" if g['calls_unknown_jobs'] else '')
            rows.append([g['agent'], g['surface'], g['model'] or '-', g['jobs'], calls] +
                        [f"{g[k]:,}" for k in COUNTS] + [f"{g['T']:,}", shown, pct(sh['fixed']), pct(sh['carry']),
                                                          pct(sh['growth']), g['ctx']['jobs_with_ctx']])
        lines.append(table(rows))
        notes = sorted({m for g in w['groups'] for m in g['W']['unknown_models']})
        if notes:
            lines.append(f"  W unknown for: {', '.join(notes)}")
    return '\n'.join(lines)


def report(args):
    usage = Path(args.usage) if args.usage else Path(args.state or state_root()) / 'usage.jsonl'
    windows = args.window or list(DEFAULT_WINDOWS)
    for w in windows:
        window_seconds(w)
    entries, source = read_entries(usage, not args.no_traces, args.trace or ())
    now = args.now if args.now is not None else time.time()
    doc = {'source': source, 'windows': windows_of(entries, windows, now)}
    print(json.dumps(doc, indent=2) if args.json else report_text(doc))


# ---- attribute: the shell's own call rule over events.jsonl --------------------------------------------------------

def u64(obj, key):
    value = obj.get(key) if isinstance(obj, dict) else None
    return value if whole(value) else 0


def codex_calls(lines):
    """(per-call input sides, decompose-style call count, skipped reasons) of one app-server events.jsonl, by
    parse.rs's rules for thread/tokenUsage/updated. The decompose-style count is every report whose total differs
    from the previous report's; the ones the shell does not count as a call each land in one skipped reason."""
    contexts, decompose, skipped = [], 0, {}
    usage, previous, turn, asked, completed = None, None, '', False, False
    for line in lines:
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if not isinstance(msg, dict):
            continue
        if 'method' not in msg:
            answer = msg.get('id')
            result = msg.get('result') if isinstance(msg.get('result'), dict) else {}
            if answer == 2:
                asked = True
            elif answer == 3:
                got = (result.get('turn') or {}).get('id') if isinstance(result.get('turn'), dict) else None
                if isinstance(got, str) and got and not turn:
                    turn = got
            continue
        params = msg.get('params') if isinstance(msg.get('params'), dict) else {}
        if msg['method'] == 'turn/completed':
            other = (params.get('turn') or {}).get('id') if isinstance(params.get('turn'), dict) else ''
            other = other if isinstance(other, str) else ''
            if not asked or (turn and other and other != turn) or completed:
                continue
            completed = True
            if not turn and other:
                turn = other
            continue
        if msg['method'] != 'thread/tokenUsage/updated':
            continue
        info = params.get('tokenUsage') if isinstance(params.get('tokenUsage'), dict) else {}
        total = info.get('total')
        differs = total != previous
        previous = total
        decompose += differs
        tagged = params.get('turnId')
        whole_counts = isinstance(total, dict) and whole(total.get('inputTokens')) and whole(total.get('outputTokens'))
        if not whole_counts or any(u64(total, k) < u64(usage, k) for k in ('inputTokens', 'outputTokens')):
            reason = 'behind_or_malformed'  # the counter rule: the last good total stays
        else:
            reason = (None if 'last' in info else 'no_last')
            reason = reason or (None if turn else 'before_turn')
            reason = reason or ('other_turn' if isinstance(tagged, str) and tagged != turn else None)
            reason = reason or ('repeat' if total == usage else None)
            usage = total
        if reason is None:
            contexts.append(u64(info['last'], 'inputTokens'))
        elif differs:
            skipped[reason] = skipped.get(reason, 0) + 1
    return contexts, decompose, skipped


def claude_calls(lines):
    """(per-call input sides, call count, no skips) of one Claude stream-json events.jsonl: the first event of each
    message id of the turn itself (not a subagent's)."""
    seen, contexts = set(), []
    for line in lines:
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if not isinstance(event, dict) or event.get('type') != 'assistant' or isinstance(event.get('parent_tool_use_id'), str):
            continue
        message = event.get('message')
        if not isinstance(message, dict) or not isinstance(message.get('id'), str) or not isinstance(message.get('usage'), dict):
            continue
        if message['id'] in seen:
            continue
        seen.add(message['id'])
        use = message['usage']
        contexts.append(u64(use, 'input_tokens') + u64(use, 'cache_read_input_tokens') + u64(use, 'cache_creation_input_tokens'))
    return contexts, len(contexts), {}


def split(contexts, base):
    """lib.rs attribute(): fixed = min(c, B), carry = max(0, min(c, first) - B), growth = max(0, c - first)."""
    first = contexts[0]
    return (sum(min(c, base) for c in contexts), sum(max(0, min(c, first) - base) for c in contexts),
            sum(max(0, c - first) for c in contexts))


def positive_int(text):
    """argparse type for a base: a positive token count."""
    n = int(text)
    if n < 1:
        raise argparse.ArgumentTypeError('a base is a positive token count')
    return n


def classify(path):
    """'codex', 'claude' or None, by the first line."""
    try:
        with open(path, encoding='utf-8', errors='replace') as f:
            first = f.readline()
    except OSError:
        return None
    if first.startswith('{"id":1'):
        return 'codex'
    try:
        head = json.loads(first)
    except ValueError:
        return None
    if isinstance(head, dict) and head.get('id') == 1 and 'method' not in head:
        return 'codex'  # the same answer to initialize, written with spaces
    return 'claude' if isinstance(head, dict) and head.get('type') == 'system' and head.get('subtype') == 'init' else None


def measured_base(usage_file, agent):
    """The median ctx_base of an agent's usage rows, or None."""
    bases = []
    try:
        for line in Path(usage_file).read_text(encoding='utf-8', errors='replace').splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if isinstance(row, dict) and row.get('agent') == agent and whole(row.get('ctx_base')):
                bases.append(row['ctx_base'])
    except OSError:
        return None
    bases.sort()
    return bases[len(bases) // 2] if bases else None


def job_agents(usage_file):
    """{job folder: the agent that ran it} from usage.jsonl rows that name both."""
    agents = {}
    try:
        for line in Path(usage_file).read_text(encoding='utf-8', errors='replace').splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if isinstance(row, dict) and isinstance(row.get('job'), str) and isinstance(row.get('agent'), str):
                agents[os.path.normpath(row['job'])] = row['agent']
    except OSError:
        pass
    return agents


def attribute(args):
    if args.base is not None and not args.agent:
        raise Usage('--base is one agent\'s base: name the agent with --agent')
    state = Path(args.state or state_root())
    shell = Path(args.shell) if args.shell else state / 'shell'
    agents = [args.agent] if args.agent else ['codex', 'claude']
    files = {a: [] for a in agents}
    # A job counts for the agent its usage row names: an owner's own agent on the codex or claude protocol (a
    # specialist) has its own base and is left out. A job without a row falls back to its protocol.
    owners = job_agents(state / 'usage.jsonl')
    others, unrowed = {a: 0 for a in agents}, {a: 0 for a in agents}
    for path in sorted(glob.glob(str(shell / '*' / 'jobs' / '*' / 'events.jsonl'))):
        kind = classify(path)
        if kind not in files:
            continue
        owner = owners.get(os.path.normpath(os.path.dirname(path)))
        if owner is None:
            unrowed[kind] += 1
        elif owner != kind:
            others[kind] += 1
            continue
        files[kind].append(path)
    out = {'shell': str(shell), 'agents': {}}
    for agent in agents:
        base = args.base if args.base is not None else measured_base(state / 'usage.jsonl', agent)
        if base is None:
            raise Usage(f'no base for {agent}: pass --base N (no usage row records a ctx_base for it)')
        extract = codex_calls if agent == 'codex' else claude_calls
        jobs, calls, decompose, total = 0, 0, 0, 0
        parts, skipped = dict.fromkeys(CTX_KEYS, 0), {}
        for path in files[agent]:
            with open(path, encoding='utf-8', errors='replace') as f:
                contexts, count, why = extract(f.read().splitlines())
            decompose += count
            for k, n in why.items():
                skipped[k] = skipped.get(k, 0) + n
            if not contexts:
                continue
            jobs += 1
            calls += len(contexts)
            total += sum(contexts)
            for k, n in zip(CTX_KEYS, split(contexts, base)):
                parts[k] += n
        out['agents'][agent] = {'base': base, 'event_files': len(files[agent]), 'other_agents_left_out': others[agent],
                                'without_usage_row': unrowed[agent], 'jobs': jobs, 'calls': calls, 'total': total,
                                **parts, 'shares': {k: round(v / total, 4) if total else None for k, v in parts.items()},
                                'surplus': sum(parts.values()) - total, 'skipped': skipped,
                                'decompose_calls': decompose,
                                'identity': f"{calls} + {sum(skipped.values())} = {calls + sum(skipped.values())}"
                                            f" (decompose-style count {decompose})"}
    print(json.dumps(out, indent=2) if args.json else attribute_text(out))


def attribute_text(doc):
    lines = [f"ledger attribute: {doc['shell']}; 0 model calls"]
    for agent, a in doc['agents'].items():
        lines += ['', f"{agent}: base {a['base']:,}; {a['event_files']} event files, {a['jobs']} jobs with calls, {a['calls']:,} calls, "
                      f"context total {a['total']:,}"]
        for k in CTX_KEYS:
            lines.append(f"  {k:7s} {a[k]:>14,d}  {pct(a['shares'][k]):>5}%")
        lines.append(f"  surplus (parts minus total) {a['surplus']:,}")
        if a['skipped']:
            lines.append('  not counted as calls: ' + ', '.join(f'{k} {v}' for k, v in sorted(a['skipped'].items())))
        lines.append(f"  identity: {a['identity']}")
        if a['other_agents_left_out'] or a['without_usage_row']:
            lines.append(f"  jobs left out as another agent's (same protocol): {a['other_agents_left_out']}; "
                         f"counted by protocol for want of a usage row: {a['without_usage_row']}")
    return '\n'.join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(prog='ledger.py', description='Offline usage ledger: tokens, weights (W) and context split. '
                                                                    'Reads files only; no model calls.')
    sub = parser.add_subparsers(dest='command', required=True)
    r = sub.add_parser('report', help='group usage.jsonl by agent, surface, model and time window')
    r.add_argument('--state', help='the Ringo state folder (default $RINGO_RELEASE_HOME or ~/.local/share/ringo)')
    r.add_argument('--usage', help='a usage.jsonl file (default <state>/usage.jsonl)')
    r.add_argument('--window', action='append', help='Nh, Nd or all; repeatable (default 24h 7d all)')
    r.add_argument('--now', type=float, help='the time to measure windows from, in epoch seconds (default: now)')
    r.add_argument('--no-traces', action='store_true', help='keep pipeline rows whole instead of expanding them from their traces')
    r.add_argument('--trace', action='append', help='a pipeline trace folder, or a folder of them; adds its attempts')
    r.add_argument('--json', action='store_true')
    a = sub.add_parser('attribute', help="split each model call's context into fixed, carry and growth from events.jsonl")
    a.add_argument('--state', help='the Ringo state folder (default $RINGO_RELEASE_HOME or ~/.local/share/ringo)')
    a.add_argument('--shell', help='the shell state folder (default <state>/shell)')
    a.add_argument('--agent', choices=['codex', 'claude'])
    a.add_argument('--base', type=positive_int, help="the agent's fixed base in tokens (default: the median ctx_base in usage.jsonl)")
    a.add_argument('--json', action='store_true')
    try:
        args = parser.parse_args(argv)
        (report if args.command == 'report' else attribute)(args)
    except Usage as e:
        print(f'ledger.py: {e}', file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    sys.exit(main())
