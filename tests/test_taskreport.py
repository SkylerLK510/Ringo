"""Offline checks of taskreport.py and the Run hooks it reads: synthetic trace records only, no model calls.

The report is a measurement artifact: every fixture here is reconciled against a hand-computed ledger, unknown
stays unknown, and the builder stays pure — same records in, same dict out, nothing written, nothing that could
flow back into a prompt.
"""
import hashlib
import json
import sys
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import harness  # noqa: E402
import taskreport  # noqa: E402


def rec(step, attempt, status, started, usage=None, **extra):
    """One trace record as harness.record and workflow.call write them."""
    r = {'step': step, 'attempt': attempt, 'backend': 'b', 'started': started, 'status': status}
    if usage is not None:
        r['usage'] = usage
    r.update(extra)
    return r


def ok(started, step='s', attempt=1, **usage):
    return rec(step, attempt, 'ok', started, usage)


class LedgerTest(unittest.TestCase):
    def test_a_hand_computed_ledger_reconciles_across_steps_and_statuses(self):
        records = [
            ok(1, 'a', 1, input=100, cache_write=0, cache_read=50, output=20, cost_usd=0.01),
            ok(2, 'b', 1, input=200, output=30, cost_usd=0.02),          # cache_read unknown: absent, not zero
            rec('c', 1, 'error', 3, {'input': 10, 'cost_usd': 0.005}),   # paid failure, output unknown
            rec('d', 1, 'error', 4, {'cost_usd': None}),                 # paid failure, no token counts at all
            rec('e', 0, 'skipped', 5),
        ]
        report = taskreport.build(records)
        self.assertEqual(report['schema_version'], 1)
        whole = report['consumption']['whole_task']
        # 100+200+10 input, 0 cache-write, 50 cache-read, 20+30 output; d counts as unknown, never as zero
        self.assertEqual(whole, {'input': 310, 'cache_write': 0, 'cache_read': 50, 'output': 50,
                                 'total': 410, 'requests': 3})
        self.assertEqual(report['consumption']['unknown_usage_requests'], 1)
        self.assertEqual(report['consumption']['untokened_calls'], 1)
        by_status = report['consumption']['by_status']
        self.assertEqual(by_status['ok'], {'input': 300, 'cache_write': 0, 'cache_read': 50, 'output': 50,
                                           'total': 400, 'requests': 2})
        self.assertEqual(by_status['error'], {'input': 10, 'cache_write': 0, 'cache_read': 0, 'output': 0,
                                              'total': 10, 'requests': 1})
        self.assertEqual([g['kind'] for g in report['coverage']['gaps']], ['unknown_counters'])
        self.assertEqual(report['outcome']['calls_failed'], 2)
        self.assertEqual(report['outcome']['calls_completed'], 4)  # every paid attempt, a failed one included

    def test_duplicates_and_out_of_order_events_are_counted_once_and_flagged(self):
        base = ok(1, input=100, output=5)
        later = ok(2, input=10, output=1)
        report = taskreport.build([base, later, dict(base), ok(0, input=1)])  # duplicate + a back-dated record
        self.assertEqual(report['consumption']['whole_task'],
                         {'input': 111, 'cache_write': 0, 'cache_read': 0, 'output': 6, 'total': 117, 'requests': 3})
        kinds = [g['kind'] for g in report['coverage']['gaps']]
        self.assertIn('duplicate_records', kinds)
        self.assertIn('out_of_order_records', kinds)

    def test_a_cumulative_counter_that_falls_is_flagged_not_summed_silently(self):
        records = [rec('s', 1, 'ok', 1, {'input': 5, 'output': 2, 'total_input_tokens': 500}),
                   rec('s', 2, 'ok', 2, {'input': 5, 'output': 2, 'total_input_tokens': 300})]
        report = taskreport.build(records)
        self.assertEqual(report['consumption']['whole_task']['input'], 10)  # the normalized per-request counts
        gaps = [g for g in report['coverage']['gaps'] if g['kind'] == 'counter_discontinuity']
        self.assertEqual(len(gaps), 1)
        self.assertEqual((gaps[0]['from'], gaps[0]['to'], gaps[0]['field']), (500, 300, 'total_input_tokens'))

    def test_negative_or_invalid_counters_are_excluded_and_flagged_not_zeroed(self):
        report = taskreport.build([ok(1, input=-5, output=3), ok(2, input=7, output=True)])
        self.assertEqual(report['consumption']['whole_task'],
                         {'input': 7, 'cache_write': 0, 'cache_read': 0, 'output': 3, 'total': 10, 'requests': 2})
        kinds = [g['kind'] for g in report['coverage']['gaps']]
        self.assertEqual(kinds.count('invalid_counter'), 2)

    def test_aggregate_subtotals_are_not_added_and_unlinked_call_overlap_is_unknown(self):
        records = [ok(1, 'parent', 1, input=1000, output=50, models=['m', 'n'], cost_usd=0.1),
                   ok(2, 'child', 1, input=100, output=5)]
        report = taskreport.build(records)
        scopes = {r['step']: r['scope'] for r in report['consumption']['requests']}
        self.assertEqual(scopes, {'parent': 'provider_turn_aggregate', 'child': 'backend_call'})
        self.assertEqual(report['consumption']['whole_task']['input'], 1100)
        self.assertIn('aggregate_attribution_unknown', [g['kind'] for g in report['coverage']['gaps']])

    def test_usage_on_a_record_that_never_settled_is_flagged(self):
        report = taskreport.build([rec('s', 1, 'started', 1, {'input': 5})])
        self.assertEqual([g['kind'] for g in report['coverage']['gaps']], ['usage_on_unsettled_record'])
        self.assertEqual(report['consumption']['whole_task']['requests'], 0)

    def test_untokened_mismatch_between_summary_and_records_is_flagged(self):
        summary = {'calls': 1, 'untokened_calls': 2, 'unfinished_calls': 0}
        report = taskreport.build([ok(1, input=1)], summary=summary)
        self.assertIn('untokened_mismatch', [g['kind'] for g in report['coverage']['gaps']])

    def test_no_records_with_spent_tokens_falls_back_to_the_run_summary_and_names_the_gap(self):
        summary = {'calls': 2, 'unfinished_calls': 0, 'input': 50, 'output': 5, 'cache_write': 0, 'cache_read': 0,
                   'untokened_calls': 0, 'unpriced_calls': 0}
        report = taskreport.build([], summary=summary)
        self.assertEqual(report['consumption']['whole_task']['input'], 50)
        self.assertIsNone(report['consumption']['whole_task']['requests'])
        self.assertIn('no_trace_records', [g['kind'] for g in report['coverage']['gaps']])


class ContextAndAmplificationTest(unittest.TestCase):
    def test_capacity_and_disabled_compaction_are_not_active_context_or_events(self):
        records = [ok(1, context_window=1000000, compaction_count=0, compacted=False),
                   ok(2, compact_threshold=800000, context_compacted=False)]
        report = taskreport.build(records)
        self.assertFalse(report['active_context']['available'])
        self.assertEqual(report['amplification']['compactions']['count'], 0)

    def test_lifetime_consumption_is_never_substituted_for_active_context(self):
        report = taskreport.build([ok(1, input=900, output=5)])
        self.assertFalse(report['active_context']['available'])
        self.assertIn('not active context', report['active_context']['note'])
        contexted = taskreport.build([ok(1, input=900, context_tokens=500), ok(2, context_tokens=900)])
        self.assertTrue(contexted['active_context']['available'])
        self.assertEqual(contexted['active_context']['latest'], 900)
        self.assertEqual(contexted['active_context']['peak'], 900)
        self.assertEqual(contexted['active_context']['source'], 'context_tokens')
        self.assertEqual(len(contexted['active_context']['trajectory']), 2)
        self.assertIn('approximate', contexted['active_context']['uncertainty'])

    def test_amplification_is_null_when_not_defined(self):
        report = taskreport.build([ok(1, input=310, output=5)], final_state='failed')
        self.assertEqual(report['amplification']['uncached_input_per_usage_record'], 310.0)
        self.assertIsNone(report['amplification']['uncached_input_at_completed_state'])
        self.assertIsNone(report['amplification']['raw_artifact_bytes'])
        accepted = taskreport.build([ok(1, input=310, output=5)], final_state='done')
        self.assertEqual(accepted['amplification']['uncached_input_at_completed_state'], 310)
        empty = taskreport.build([])
        self.assertIsNone(empty['amplification']['uncached_input_per_usage_record'])

    def test_tool_artifact_bytes_come_from_the_trace_and_stay_out_of_tokens(self):
        with tempfile.TemporaryDirectory() as d:
            steps = Path(d, 'steps')
            steps.mkdir()
            (steps / 's.1.raw.txt').write_bytes(b'x' * 77)
            report = taskreport.build([ok(1, input=5)], trace=d)
            self.assertEqual(report['amplification']['raw_artifact_bytes'], 77)
            self.assertEqual(report['amplification']['raw_artifacts'], 1)
        self.assertIn('outside every token count', report['amplification']['notes'][1])

    def test_compactions_are_zero_only_with_an_explicit_note(self):
        unknown = taskreport.build([ok(1, input=5)])
        self.assertEqual(unknown['amplification']['compactions']['count'], 0)
        self.assertIn('none was observed', unknown['amplification']['compactions']['note'])
        marked = taskreport.build([ok(1, input=5), rec('s', 1, 'ok', 2, {'input': 5, 'compacted': True})])
        self.assertEqual(marked['amplification']['compactions']['count'], 1)

    def test_blockers_reach_the_report_under_repeated_failure_outputs(self):
        blocker = {'signature': 'error:abc', 'count': 4, 'step': 's', 'attempt': 1, 'sha256': 'abc',
                   'total_bytes': 10, 'at': 1.0}
        report = taskreport.build([rec('s', 1, 'error', 1)], blockers=[blocker])
        reads = report['amplification']['repeated_failure_outputs']
        self.assertEqual((reads['signatures'], reads['repeat_events']), (1, 3))
        self.assertEqual(report['amplification']['blockers'], [blocker])


class OutcomeTest(unittest.TestCase):
    def test_failures_and_stops_are_counted_without_usage(self):
        report = taskreport.build([rec('a', 1, 'error', 1),
                                   rec('b', 1, 'fatal', 2, stopped='timeout')])
        self.assertEqual(report['outcome']['calls_failed'], 2)
        self.assertEqual(report['outcome']['stopped_calls'], 1)
        self.assertEqual(report['consumption']['unknown_usage_requests'], 1)

    def test_missing_status_does_not_break_mixed_status_accounting(self):
        report = taskreport.build([{'usage': {'input': 99}}, ok(1, input=5)])
        self.assertEqual(report['consumption']['whole_task']['input'], 5)
        self.assertIn('unknown', report['consumption']['by_status'])
        self.assertIn('invalid_status', [g['kind'] for g in report['coverage']['gaps']])

    def test_cost_is_reported_only_where_supportable(self):
        priced = taskreport.build([ok(1, input=1, cost_usd=0.02)], summary={'calls': 1, 'unfinished_calls': 0,
                                                                           'unpriced_calls': 0, 'elapsed_s': 1.0},
                                  cost_usd=0.02)
        self.assertEqual(priced['outcome']['cost_usd'], 0.02)
        self.assertIn('not a monetary invoice', priced['outcome']['notes'][0])
        unpriced = taskreport.build([rec('s', 1, 'error', 1)], summary={'calls': 1, 'unfinished_calls': 0,
                                                                       'unpriced_calls': 1, 'elapsed_s': 1.0},
                                    cost_usd=None)
        self.assertIsNone(unpriced['outcome']['cost_usd'])
        self.assertTrue(any('not supportable' in n for n in unpriced['outcome']['notes']))

    def test_budget_info_and_the_incomplete_outcome_note(self):
        summary = {'input': 120, 'cache_write': 0, 'cache_read': 0, 'output': 0}
        report = taskreport.build([ok(1, input=120)], summary=summary, max_tokens=100, overshoot_calls=1,
                                  final_state='failed')
        self.assertEqual(report['outcome']['budget'], {'max_tokens': 100, 'recorded_tokens': 120, 'overshoot': 1})
        self.assertTrue(any('incomplete outcome' in n for n in report['outcome']['notes']))

    def test_the_builder_is_pure_and_never_echoes_record_text(self):
        sentinel = 'SECRET-MATERIAL-TEXT'
        records = [rec('s', 1, 'error', 1, {'cost_usd': None}, error=f'boom {sentinel}')]
        with tempfile.TemporaryDirectory() as d:
            before = set(Path(d).iterdir())
            first, second = taskreport.build(records), taskreport.build(records)
            self.assertEqual(first, second)  # same records in, same report out
            self.assertEqual(set(Path(d).iterdir()), before)  # and nothing was written anywhere
        self.assertNotIn(sentinel, json.dumps(first))

    def test_unparseable_trace_lines_become_gaps(self):
        report = taskreport.build([None, ok(1, input=1)])
        self.assertEqual(report['consumption']['whole_task']['input'], 1)
        self.assertIn('unparseable_trace_line', [g['kind'] for g in report['coverage']['gaps']])

    def test_identifiers_and_schema_groups_are_present(self):
        report = taskreport.build([ok(1, input=1)], task_id='t', started=123.5)
        self.assertEqual(report['schema_version'], taskreport.SCHEMA_VERSION)
        self.assertEqual(report['identifiers']['task_id'], 't')
        self.assertEqual(report['identifiers']['started'], 123.5)
        for group in ('identifiers', 'consumption', 'active_context', 'amplification', 'outcome', 'coverage'):
            self.assertIn(group, report)


class RunHookTest(unittest.TestCase):
    def test_interleaved_blockers_keep_one_latest_entry_per_signature(self):
        run = harness.Run(blocker_threshold=1)
        for attempt, output in enumerate(('a', 'b', 'a', 'b', 'a'), 1):
            run.record({'step': 's', 'attempt': attempt, 'status': 'error'}, raw=output)
        self.assertEqual([b['count'] for b in run.blockers], [3, 2])
        self.assertEqual(len({b['signature'] for b in run.blockers}), 2)

    def test_report_failure_does_not_prevent_final_state(self):
        with tempfile.TemporaryDirectory() as d:
            run = harness.Run(Path(d, 't'))
            with patch.object(run, 'task_report', side_effect=ValueError('invalid telemetry')):
                with patch('sys.stderr'):
                    run.state('done')
            self.assertEqual(json.loads(Path(d, 't', 'state.json').read_text())['status'], 'done')

    def test_the_whole_output_lands_in_the_artifact_and_the_excerpt_names_the_omission(self):
        with tempfile.TemporaryDirectory() as d:
            head = b'x' * 600_000 + b'\nIMPORTANT-ERROR-IN-THE-MIDDLE\n' + b'y' * 500_000 + b'\nTAIL-ERROR\n'
            run = harness.Run(Path(d, 't'), blocker_threshold=None)
            run.record({'step': 's', 'attempt': 1, 'status': 'error', 'error': 'boom'}, raw=head.decode())
            artifact = Path(d, 't', 'steps', 's.1.raw.txt')
            data = artifact.read_bytes()
            self.assertEqual(data, head)  # the whole output, middle and end included
            rows = [json.loads(x) for x in Path(d, 't', 'trace.jsonl').read_text().splitlines()]
            excerpt = rows[-1]['raw_excerpt']
            self.assertEqual(excerpt['sha256'], hashlib.sha256(data).hexdigest())
            self.assertEqual(excerpt['total_bytes'], len(data))
            self.assertEqual(excerpt['omitted_bytes'], len(data) - harness.RAW_LIMIT)
            self.assertIn('artifact holds the whole output', excerpt['omission_note'])
            self.assertIn('IMPORTANT-ERROR-IN-THE-MIDDLE', excerpt['head'])
            self.assertNotIn('TAIL-ERROR', excerpt['head'])  # past the excerpt, recoverable via artifact + digest
            self.assertIn('TAIL-ERROR', data.decode())

    def test_repeated_blockers_escalate_and_distinct_signatures_count_separately(self):
        with tempfile.TemporaryDirectory() as d:
            run = harness.Run(Path(d, 't'), blocker_threshold=3)
            for attempt in (1, 2, 3, 4):
                run.record({'step': 's', 'attempt': attempt, 'status': 'error', 'error': 'same failure'},
                           raw='same output')
            rows = [json.loads(x) for x in Path(d, 't', 'trace.jsonl').read_text().splitlines()]
            flags = [r.get('blocker_repeat') for r in rows if r['status'] == 'error']
            self.assertEqual(flags, [None, None, 3, 4])  # escalation from the threshold, never before
            self.assertEqual(len(run.blockers), 1)
            self.assertEqual(run.blockers[0]['count'], 4)
            for attempt in (1, 2):  # a different output: its own counter, no flag under the threshold
                run.record({'step': 'other', 'attempt': attempt, 'status': 'error', 'error': 'different'},
                           raw='different output')
            self.assertEqual(len(run.blockers), 1)
            rows = [json.loads(x) for x in Path(d, 't', 'trace.jsonl').read_text().splitlines()]
            self.assertIsNone(rows[-1].get('blocker_repeat'))
            report = run.task_report()
            self.assertEqual(report['amplification']['repeated_failure_outputs']['signatures'], 1)

    def test_blocker_threshold_none_disables_detection(self):
        run = harness.Run(blocker_threshold=None)
        for _ in range(5):
            run.record({'step': 's', 'attempt': 1, 'status': 'error', 'error': 'x'}, raw='y')
        self.assertEqual(run.blockers, [])

    def test_blocker_threshold_is_validated(self):
        for bad in (0, -1, 'x', 1.5):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                harness.Run(blocker_threshold=bad)


class TokenBudgetTest(unittest.TestCase):
    def test_partial_usage_records_known_overshoot_and_running_budget(self):
        with tempfile.TemporaryDirectory() as d:
            run = harness.Run(Path(d, 't'), max_tokens=100)
            run.record({'step': 's', 'attempt': 1, 'status': 'ok', 'usage': {'input': 120}})
            state = json.loads(Path(d, 't', 'state.json').read_text())
            self.assertEqual(state['budget']['recorded_tokens'], 120)
            self.assertEqual(state['budget']['overshoot'], 1)
            with self.assertRaises(harness.BudgetExceeded):
                run.reserve()

    def test_invalid_counter_values_stay_unknown(self):
        for bad in (None, True, -1, 1.5):
            with self.subTest(bad=bad):
                run = harness.Run(max_tokens=100)
                run.record({'step': 's', 'attempt': 1, 'status': 'ok',
                            'usage': {'input': bad, 'output': 1, 'cache_read': 0, 'cache_write': 0}})
                self.assertEqual(run.tokens['input'], 0)
                with self.assertRaises(harness.BudgetExceeded):
                    run.reserve()

    def test_the_cap_stops_admitting_calls_once_reached(self):
        run = harness.Run(max_tokens=100)
        run.reserve()
        run.record({'step': 's', 'attempt': 1, 'status': 'ok',
                    'usage': {'input': 90, 'cache_write': 0, 'cache_read': 0, 'output': 10}})
        with self.assertRaisesRegex(harness.BudgetExceeded, 'token cap reached'):
            run.reserve()

    def test_an_untokened_call_makes_the_cap_unkeepable(self):
        run = harness.Run(max_tokens=100)
        run.reserve()
        run.record({'step': 's', 'attempt': 1, 'status': 'ok', 'usage': None})
        with self.assertRaisesRegex(harness.BudgetExceeded, 'cannot be kept'):
            run.reserve()

    def test_an_in_flight_overshoot_is_recorded_and_kills_nothing(self):
        with tempfile.TemporaryDirectory() as d:
            run = harness.Run(Path(d, 't'), max_tokens=100)
            run.reserve()
            run.record({'step': 's', 'attempt': 1, 'status': 'ok',
                        'usage': {'input': 120, 'cache_write': 0, 'cache_read': 0, 'output': 0}})  # in flight
            rows = [json.loads(x) for x in Path(d, 't', 'trace.jsonl').read_text().splitlines()]
            self.assertEqual(rows[-1]['budget_overshoot_tokens'], 20)
            self.assertEqual(run.budget_overshoot_calls, 1)
            run.state('done', result='r')
            state = json.loads(Path(d, 't', 'state.json').read_text())
            self.assertEqual(state['budget']['overshoot'], 1)
            self.assertEqual(state['budget']['recorded_tokens'], 120)
            report = json.loads(Path(d, 't', 'task-report.json').read_text())
            self.assertEqual(report['outcome']['budget']['overshoot'], 1)
            self.assertEqual(report['schema_version'], 1)

    def test_budget_stays_out_of_today_s_behavior(self):
        with tempfile.TemporaryDirectory() as d:
            run = harness.Run(Path(d, 't'))
            run.record({'step': 's', 'attempt': 1, 'status': 'ok', 'usage': {'input': 1_000_000, 'output': 0}})
            run.state('done', result='r')
            state = json.loads(Path(d, 't', 'state.json').read_text())
            self.assertNotIn('budget', state)
            self.assertNotIn('budget', json.loads(Path(d, 't', 'task-report.json').read_text())['outcome'])
            run2 = harness.Run()
            run2.reserve()  # no cap: many reservations are still admitted
            self.assertEqual(run2.summary()['calls'], 1)

    def test_max_tokens_is_validated(self):
        for bad in (-1, 'x', True, 1.0):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                harness.Run(max_tokens=bad)

    def test_the_report_is_written_once_at_a_terminal_state_and_never_read_back(self):
        with tempfile.TemporaryDirectory() as d:
            run = harness.Run(Path(d, 't'))
            run.record({'step': 's', 'attempt': 1, 'status': 'ok', 'usage': {'input': 5, 'output': 1}})
            trace_before = Path(d, 't', 'trace.jsonl').read_bytes()
            run.state('done', result='r')
            self.assertEqual(Path(d, 't', 'trace.jsonl').read_bytes(), trace_before)  # read-only derivation
            report = json.loads(Path(d, 't', 'task-report.json').read_text())
            self.assertEqual(report['schema_version'], 1)
            self.assertEqual(report['outcome']['final_state'], 'done')
            self.assertEqual(report['consumption']['whole_task']['input'], 5)
            self.assertEqual(json.loads(Path(d, 't', 'state.json').read_text())['task_report'], 'task-report.json')
            self.assertNotIn('SECRET', json.dumps(report))

    def test_a_run_without_a_trace_reports_its_own_counters_and_names_the_gap(self):
        run = harness.Run()
        run.reserve()
        run.record({'step': 's', 'attempt': 1, 'status': 'ok', 'usage': {'input': 5, 'output': 1}})
        report = run.task_report()
        self.assertEqual(report['consumption']['whole_task']['input'], 5)
        self.assertIsNone(report['consumption']['whole_task']['requests'])
        self.assertIn('no_trace_records', [g['kind'] for g in report['coverage']['gaps']])
        self.assertIsNone(report['identifiers']['task_id'])


if __name__ == '__main__':
    unittest.main()
