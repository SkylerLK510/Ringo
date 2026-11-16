"""Whole-task, read-only measurement report for one run (schema_version 1).

The report is a private measurement artifact for the person and later offline analysis, never a prompt: nothing
reads it back into a model-facing message. It is derived only from the trace records a run already wrote
(trace.jsonl) plus the run's own summary, so writing it cannot change what the run measured and stays possible
after the process is gone. Groups: identifiers; whole-task consumption in nonoverlapping accounting scopes (a
usage dict that already aggregates child usage is counted once and separately reported child usage is never
added again); active context (provider-reported estimates only — lifetime consumption is never substituted for
it); amplification; outcome; coverage. Every unknown is an explicit null with a note, never a zero.
"""
import getpass
import json
import re
import socket
from pathlib import Path

SCHEMA_VERSION = 1
TOKEN_KEYS = ('input', 'cache_write', 'cache_read', 'output')
PAID = ('ok', 'malformed', 'error')  # every attempt is paid for, a failed one too (as harness.py records it)
FAILED = ('error', 'fatal')
ACCEPTED = ('done', 'ready_for_review')  # final states that mean the task's output was accepted by its checks
CONTEXT_KEY = re.compile(r'context', re.I)
COMPACT_KEY = re.compile(r'compact', re.I)
CUMULATIVE_KEY = re.compile(r'total|cumulative', re.I)
AGGREGATE_KEY = 'models'  # claude-style usage names the models of every sub-call its own totals already include

NORMALIZATION_NOTE = ("'input' is uncached input: the capture paths already separate cached tokens (the Codex "
                      "path subtracts cached_input_tokens from the provider's total input; the Claude path's "
                      "input_tokens excludes the cache fields by the provider's own accounting), so cache-write "
                      "and cache-read are never added into it.")
SCOPE_NOTE = ("a usage dict that names its models (claude-style 'models') is the provider's aggregate for the "
              "whole CLI turn, including any native child calls it covered: it is counted once, and separately "
              "reported child usage is never added again. Every other usage dict covers exactly one provider "
              "request whose tokens are its own.")
RAW_FIELDS_NOTE = ("requests[].raw_usage keeps each usage dict exactly as the backend reported it (raw provider "
                   "field names and values), with a warning-text list bounded to a warning_count.")


def _nonneg(value):
    """value when it is a non-negative integer (a bool is not a count), else None."""
    return value if type(value) is int and value >= 0 else None


def _dedupe(records, gaps):
    """Trace records in file order with exact duplicates collapsed, and the ordering problems named. A
    duplicated event line must not be counted twice; consumption itself is order-independent."""
    kept, seen, duplicates, started = [], set(), 0, []
    for rec in records:
        if not isinstance(rec, dict):
            gaps.append({'kind': 'unparseable_trace_line', 'note': 'a trace line that is not a JSON object was skipped'})
            continue
        identity = json.dumps(rec, sort_keys=True, default=str)
        if identity in seen:
            duplicates += 1
            continue
        seen.add(identity)
        kept.append(rec)
        if type(rec.get('started')) in (int, float):
            started.append(rec['started'])
    if duplicates:
        gaps.append({'kind': 'duplicate_records', 'dropped': duplicates,
                     'note': 'identical trace lines were collapsed so a duplicated event is not counted twice'})
    if any(later < earlier for earlier, later in zip(started, started[1:])):
        gaps.append({'kind': 'out_of_order_records',
                     'note': 'records carry out-of-order timestamps; consumption is order-independent'})
    return kept


def _counters(usage):
    """({category: count or None}, problems) for one usage dict: an absent or invalid count stays unknown."""
    categories, problems = {}, []
    for key in TOKEN_KEYS:
        value = usage.get(key)
        if _nonneg(value) is None:
            categories[key] = None
            if value is not None:
                problems.append(f'{key} is not a non-negative integer')
        else:
            categories[key] = value
    return categories, problems


def _raw_usage(usage):
    """The usage dict as captured, with a warning-text list bounded to a count (the report must stay bounded)."""
    raw = {k: v for k, v in usage.items() if k != 'warnings'}
    if isinstance(usage.get('warnings'), list):
        raw['warning_count'] = len(usage['warnings'])
    return raw


def _rows(kept, gaps):
    """One row per kept record that carries a usage dict, with normalized counters, raw usage and scope."""
    rows = []
    for rec in kept:
        usage = rec.get('usage')
        if not isinstance(usage, dict):
            continue
        counters, problems = _counters(usage)
        row = {'step': rec.get('step'), 'attempt': rec.get('attempt'), 'status': rec.get('status'),
               'backend': rec.get('backend'), 'model': rec.get('model'), 'started': rec.get('started'),
               'counters': counters, 'raw_usage': _raw_usage(usage),
               'scope': 'provider_turn_aggregate' if AGGREGATE_KEY in usage else 'single_request'}
        if rec.get('stopped'):
            row['stopped'] = rec['stopped']
        if rec.get('status') not in PAID:
            row['unsettled'] = True  # its counts are kept but never trusted into a ledger
        rows.append(row)
        for problem in problems:
            gaps.append({'kind': 'invalid_counter', 'step': row['step'], 'attempt': row['attempt'], 'detail': problem})
        if any(v is not None for v in counters.values()) and rec.get('status') not in PAID:
            gaps.append({'kind': 'usage_on_unsettled_record', 'step': row['step'], 'attempt': row['attempt'],
                         'status': rec.get('status'),
                         'note': 'token counts on a record that never settled as paid are kept as raw fields but '
                                 'excluded from every ledger'})
    _discontinuities(rows, gaps)
    return rows


def _discontinuities(rows, gaps):
    """Raw fields that look cumulative must never fall between the requests of one task: a fall means the
    provider restarted a counter (or the capture mixed cumulative and incremental values), so it is flagged
    instead of being summed silently."""
    last = {}
    for row in rows:
        for field, value in row['raw_usage'].items():
            if CUMULATIVE_KEY.search(str(field)) and type(value) is int and value >= 0:
                key = (row['step'], field)  # across a step's attempts: one provider request sequence per step
                if key in last and value < last[key]:
                    gaps.append({'kind': 'counter_discontinuity', 'step': row['step'], 'attempt': row['attempt'],
                                 'field': field, 'from': last[key], 'to': value,
                                 'note': 'a cumulative-style counter fell between requests; the ledger sums the '
                                         'normalized per-request counts, never the raw cumulative fields'})
                last[key] = max(value, last.get(key, value))


def _ledger(rows):
    """({category: sum}, requests, unknown_inputs) over rows: unknown counts are skipped by the sums and named,
    never read as zero."""
    sums = dict.fromkeys(TOKEN_KEYS, 0)
    requests, unknown_inputs = 0, 0
    for row in rows:
        if row.get('unsettled'):
            continue
        known = [row['counters'][k] for k in TOKEN_KEYS if row['counters'][k] is not None]
        if known:
            requests += 1
            for k in TOKEN_KEYS:
                sums[k] += row['counters'][k] or 0
            missing = [k for k in TOKEN_KEYS if row['counters'][k] is None]
            if missing:
                unknown_inputs += 1
    return {**sums, 'total': sum(sums.values()), 'requests': requests}, unknown_inputs


def _active_context(kept):
    """Provider-reported active-context estimates, if any record carries one. Lifetime consumption is never
    substituted for active context."""
    estimates = []
    for rec in kept:
        candidates = dict(rec)
        if isinstance(rec.get('usage'), dict):
            candidates.update(rec['usage'])
        for key, value in candidates.items():
            if CONTEXT_KEY.search(str(key)) and _nonneg(value) is not None:
                estimates.append({'step': rec.get('step'), 'attempt': rec.get('attempt'), 'source': key,
                                  'value': value, 'started': rec.get('started')})
    if not estimates:
        return {'available': False,
                'note': 'no record in this trace carries a provider-reported active-context estimate; lifetime '
                        'consumption is not active context (it counts every token the task ever spent, not what '
                        'the next call would load).'}
    values = [e['value'] for e in estimates]
    return {'available': True, 'latest': estimates[-1]['value'], 'source': estimates[-1]['source'],
            'peak': max(values), 'trajectory': estimates,
            'uncertainty': 'provider estimates are approximate: they are sampled at different moments of each '
                           'call and can exclude prompt parts the provider does not report'}


def _compactions(kept, requests):
    events = [rec for rec in kept if isinstance(rec, dict) and
              any(COMPACT_KEY.search(str(k)) for k in list(rec) + list(rec.get('usage') or {}))]
    if not events:
        return {'count': 0, 'requests_between': None,
                'note': 'no record in this trace marks a compaction; 0 means none was observed, not that none '
                        'happened inside the provider'}
    return {'count': len(events), 'requests_between': round(requests / len(events), 1) if requests else None,
            'note': 'compaction records found in the trace'}


def _tool_artifacts(trace):
    """(bytes, count) of the trace's steps/*.raw.txt artifacts, or (None, 0) without a trace: these files hold
    bytes of tool output, which are outside every token count."""
    if not trace:
        return None, 0
    try:
        folder = Path(trace) / 'steps'
        files = sorted(folder.glob('*.raw.txt')) if folder.is_dir() else []
        return sum(f.stat().st_size for f in files), len(files)
    except OSError:
        return None, 0


def build(records, summary=None, blockers=(), final_state='unknown', trace=None, max_tokens=None,
          overshoot_calls=None, cost_usd=None, task_id=None, started=None):
    """The report dict for these trace records. Pure over its inputs apart from stat-ing the trace's raw
    artifacts when trace is given: same records in, same report out; nothing is written."""
    summary = dict(summary or {})
    gaps, exclusions, notes = [], [], []
    kept = _dedupe(records if records is not None else [], gaps)
    rows = _rows(kept, gaps)
    ledger, unknown_inputs = _ledger(rows)
    if not rows and (summary.get('calls') or 0) > 0:
        gaps.append({'kind': 'no_trace_records',
                     'note': 'no trace records were available; per-request detail is unknown and the sums below '
                             'come only from the run summary'})
        if any(summary.get(k) for k in TOKEN_KEYS):  # the run's own counters are real measurements, so they stand
            ledger = {k: summary.get(k) or 0 for k in TOKEN_KEYS}
            ledger.update(total=sum(ledger.values()), requests=None)
    by_status = {}
    for row in rows:
        by_status.setdefault(row['status'], []).append(row)
    statuses = {status: _ledger(group)[0] for status, group in sorted(by_status.items())}
    paid_rows = [row for row in rows if row['status'] in PAID]

    def _has_tokens(rec):
        usage = rec.get('usage')
        return isinstance(usage, dict) and any(_nonneg(usage.get(k)) is not None for k in TOKEN_KEYS)
    # a paid record whose usage carries no token fields — or none at all — is an unknown, never a zero request
    unknown_requests = sum(1 for rec in kept if rec.get('status') in PAID and not _has_tokens(rec))
    if rows and summary.get('untokened_calls') not in (None, unknown_requests):
        gaps.append({'kind': 'untokened_mismatch', 'records': unknown_requests,
                     'summary': summary['untokened_calls'],
                     'note': 'the run summary and the trace disagree about calls that reported no token counts'})
    if unknown_inputs:
        gaps.append({'kind': 'unknown_counters', 'requests': unknown_inputs,
                     'note': 'these requests reported some categories but not others; the sums skip the unknown '
                             'categories, so they understate those requests'})
    whole = dict(ledger)
    uncached = whole.get('input') or 0
    artifact_bytes, artifact_count = _tool_artifacts(trace)
    consumed = {
        'whole_task': whole,
        'by_status': statuses,
        'requests': rows,
        'normalization': {'note': NORMALIZATION_NOTE, 'raw_fields_kept': RAW_FIELDS_NOTE,
                          'accounting_scopes': SCOPE_NOTE},
        'unknown_usage_requests': unknown_requests,
        'untokened_calls': summary.get('untokened_calls', unknown_requests),
    }
    any_aggregate = any(row['scope'] == 'provider_turn_aggregate' for row in rows)
    if any_aggregate:
        exclusions.append('usage marked provider_turn_aggregate already includes every child call its provider '
                          'made inside the turn; such child usage is never added again (nonoverlapping scopes)')
    amplification = {
        'input_per_request': round(uncached / whole['requests'], 1) if whole.get('requests') else None,
        'cumulative_input_per_accepted_call': uncached if final_state in ACCEPTED else None,
        'tool_result_bytes': artifact_bytes,
        'raw_artifacts': artifact_count,
        'repeated_unchanged_reads': {'signatures': len({b.get('signature') for b in blockers}),
                                     'repeat_events': sum(max(0, b.get('count', 1) - 1) for b in blockers),
                                     'note': 'signatures hash the failure output (and any unchanged-file hint the '
                                             'record carries); unchanged-file evidence is not recorded on its own'},
        'blockers': [dict(b) for b in blockers],
        'compactions': _compactions(kept, whole.get('requests') or 0),
        'notes': ['cumulative_input_per_accepted_call is null unless the final state is an accepted outcome',
                  'tool_result_bytes counts artifact bytes, not tokens; they are outside every token count'],
    }
    stopped = sum(1 for row in rows if row.get('stopped'))
    outcome = {
        'final_state': final_state,
        'calls_completed': (summary.get('calls', 0) - summary.get('unfinished_calls', 0))
                           if summary.get('calls') is not None else sum(1 for r in paid_rows),
        'calls_failed': sum(1 for row in rows if row['status'] in FAILED),
        'stopped_calls': stopped,
        'elapsed_seconds': summary.get('elapsed_s'),
        'cost_usd': cost_usd,
        'unpriced_calls': summary.get('unpriced_calls'),
        'admission_refusals': summary.get('admission_refusals'),
        'incidents': summary.get('incidents'),
        'notes': ['cost_usd is the provider-reported price of priced calls only; subscription token usage is '
                  'not a monetary invoice',
                  'acceptance checks and constraint retention are not recorded by this harness and stay unknown',
                  'a budget stop is an incomplete outcome; the caller decides the final state'],
    }
    if max_tokens is not None:
        outcome['budget'] = {'max_tokens': max_tokens,
                             'recorded_tokens': sum(summary.get(k) or 0 for k in TOKEN_KEYS),
                             'overshoot': overshoot_calls}
    if cost_usd is None and summary.get('unpriced_calls'):
        outcome['notes'].append('cost_usd is null because some calls reported no price, so the total is not '
                                'supportable')
    exclusions += ['the task report is a private artifact and is never sent to a model',
                   'tool-result artifacts hold bytes, not tokens; they are outside every token count',
                   'subscription token usage is not a monetary invoice']
    notes += ['every unknown is null with a note, never zero',
              'identifiers for experiment, job, thread and parent are not recorded by this harness and stay '
              'unknown',
              'provider-reported usage is the only source for every token category here']
    return {
        'schema_version': SCHEMA_VERSION,
        'identifiers': {'task_id': task_id, 'started': started,
                        'hostname': socket.gethostname() or None,
                        'user': getpass.getuser() if getpass.getuser() else None},
        'consumption': consumed,
        'active_context': _active_context(kept),
        'amplification': amplification,
        'outcome': outcome,
        'coverage': {'gaps': gaps, 'exclusions': exclusions, 'notes': notes},
    }
