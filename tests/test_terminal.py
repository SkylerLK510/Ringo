"""Terminal behavior and honest wall-clock throughput, with offline backends only."""
import io
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
from unittest import mock

import harness
import workflow


USAGE = {'input': 100, 'cache_write': 0, 'cache_read': 40, 'output': 20, 'cost_usd': None}


class ThroughputTest(unittest.TestCase):
    def test_overlapping_calls_use_run_wall_time_not_summed_call_times(self):
        run = harness.Run()
        for _ in range(2):
            run.reserve()
            run.record({'status': 'ok', 'usage': USAGE, 'seconds': 2})
        summary = run.summary(elapsed_s=2.5)
        self.assertEqual(summary['output'], 40)
        self.assertEqual(summary['elapsed_s'], 2.5)
        self.assertEqual(summary['output_tokens_per_second'], 16)

    def test_retries_include_failed_and_malformed_attempt_tokens_and_time(self):
        run = harness.Run()
        for status in ('error', 'malformed', 'ok'):
            run.reserve()
            run.record({'status': status, 'usage': USAGE, 'seconds': 1})
        self.assertEqual(run.summary(elapsed_s=4)['output_tokens_per_second'], 15)

    def test_unknown_or_unfinished_calls_do_not_present_partial_rate_as_complete(self):
        for record in (None, {'status': 'error'}, {'status': 'ok', 'usage': {'cost_usd': .1}}):
            with self.subTest(record=record):
                run = harness.Run()
                run.reserve()
                run.record({'status': 'ok', 'usage': USAGE})
                run.reserve()
                if record:
                    run.record(record)
                self.assertIsNone(run.summary(elapsed_s=3)['output_tokens_per_second'])
        self.assertIsNone(harness.Run().summary(elapsed_s=3)['output_tokens_per_second'])

    def test_zero_output_is_distinct_from_unreported_output(self):
        run = harness.Run()
        run.reserve()
        run.record({'status': 'ok', 'usage': {**USAGE, 'output': 0}})
        self.assertEqual(run.summary(elapsed_s=2)['output_tokens_per_second'], 0)
        self.assertIsNone(run.summary(elapsed_s=0)['output_tokens_per_second'])

    def test_missing_provider_output_counts_remain_unknown(self):
        for count in (None, True, -1, '20'):
            with self.subTest(count=count):
                usage = harness.claude_usage({'usage': {'input_tokens': 10, 'output_tokens': count}})
                self.assertNotIn('output', usage)
                events = [json.dumps({'type': 'item.completed', 'item': {'type': 'agent_message', 'text': 'ok'}}),
                          json.dumps({'type': 'turn.completed', 'usage': {'input_tokens': 10, 'output_tokens': count}})]
                _, usage = harness.codex_reply('\n'.join(events))
                self.assertNotIn('output', usage)

    def test_output_only_usage_preserves_output_but_marks_run_accounting_incomplete(self):
        events = [json.dumps({'type': 'item.completed', 'item': {'type': 'agent_message', 'text': 'ok'}}),
                  json.dumps({'type': 'turn.completed', 'usage': {'output_tokens': 20}})]
        _, usage = harness.codex_reply('\n'.join(events))
        self.assertEqual(usage['output'], 20)
        self.assertNotIn('input', usage)
        run = harness.Run()
        run.reserve()
        run.record({'status': 'ok', 'usage': usage})
        self.assertEqual(run.summary(elapsed_s=2)['untokened_calls'], 1)
        self.assertIsNone(run.summary(elapsed_s=2)['output_tokens_per_second'])

    def test_trace_keeps_per_attempt_rates_without_changing_token_counts(self):
        with tempfile.TemporaryDirectory() as temp:
            run = harness.Run(Path(temp) / 'trace')
            run.reserve()
            run.record({'step': 'review', 'attempt': 1, 'status': 'error', 'usage': USAGE, 'seconds': .5})
            run.record({'step': 'review', 'attempt': 2, 'status': 'error', 'seconds': .5})
            records = [json.loads(line) for line in (run.trace / 'trace.jsonl').read_text().splitlines()]
            self.assertEqual(records[0]['output_tokens_per_second'], 40)
            self.assertEqual(records[0]['usage'], USAGE)
            self.assertIsNone(records[1]['output_tokens_per_second'])


class TTY(io.StringIO):
    def isatty(self):
        return True


class TerminalTest(unittest.TestCase):
    def test_work_cli_keeps_report_on_stdout_and_accepts_display_flags(self):
        run = harness.Run()
        report = {'status': 'ready_for_review', 'final': '/tmp/fixture/final', 'rounds': [],
                  'elapsed_s': 2, 'usage': run.summary(elapsed_s=2)}
        with mock.patch.object(workflow, 'run', return_value=report) as work, \
                mock.patch('sys.stdout', new_callable=io.StringIO) as out, \
                mock.patch('sys.stderr', new_callable=io.StringIO) as err:
            code = workflow.main(['--spec', 'fixture.json', '--out', '/tmp/fixture',
                                  '--progress', 'always', '--summary-format', 'text'])
        self.assertEqual(code, 0)
        work.assert_called_once_with('fixture.json', '/tmp/fixture', progress='always')
        self.assertEqual(json.loads(out.getvalue()), workflow.outcome(report, '/tmp/fixture'))
        self.assertIn('output tok/s unknown', err.getvalue())

    def test_work_cli_interrupt_returns_130(self):
        with mock.patch.object(workflow, 'run', side_effect=KeyboardInterrupt), \
                mock.patch('sys.stderr', new_callable=io.StringIO) as err:
            code = workflow.main(['--spec', 'fixture.json', '--out', '/tmp/fixture'])
        self.assertEqual(code, 130)
        self.assertIn(f'interrupted; report: {Path("/tmp/fixture") / "report.json"}', err.getvalue())

    def test_redirected_output_is_quiet_by_default_and_summary_stays_json(self):
        stream, run = io.StringIO(), harness.Run()
        with harness.TerminalProgress(run, stream=stream):
            run.reserve()
            run.record({'step': 'review', 'attempt': 1, 'status': 'started'})
            run.record({'step': 'review', 'attempt': 1, 'status': 'ok', 'usage': USAGE, 'seconds': 2})
        self.assertEqual(stream.getvalue(), '')
        with mock.patch('sys.stderr', stream):
            harness.print_summary(run.summary(elapsed_s=2))
        summary = json.loads(stream.getvalue().removeprefix('harness: '))
        self.assertEqual(summary['output_tokens_per_second'], 10)

    def test_forced_progress_when_redirected_has_steps_and_no_terminal_controls(self):
        stream, run = io.StringIO(), harness.Run()
        with harness.TerminalProgress(run, mode='always', stream=stream):
            run.record({'step': 'review', 'attempt': 2, 'status': 'started'})
            run.record({'step': 'review', 'attempt': 2, 'status': 'ok', 'usage': USAGE, 'seconds': 2})
        self.assertIn('review (attempt 2) started', stream.getvalue())
        self.assertIn('review ok | 2.00s | 10.0 output tok/s (wall time)', stream.getvalue())
        self.assertNotIn('\033', stream.getvalue())
        self.assertNotIn('\r', stream.getvalue())

    def test_live_terminal_refreshes_while_call_is_silent_and_cleans_up_on_interrupt(self):
        refreshed = threading.Event()
        class ObservedTTY(TTY):
            def write(self, text):
                if threading.current_thread() is not threading.main_thread():
                    refreshed.set()
                return super().write(text)
        stream, run = ObservedTTY(), harness.Run()
        with mock.patch.dict(os.environ, {'TERM': 'xterm'}):
            progress = harness.TerminalProgress(run, stream=stream)
            with self.assertRaises(KeyboardInterrupt), progress:
                run.record({'step': 'review', 'attempt': 1, 'status': 'started'})
                self.assertTrue(refreshed.wait(2), 'no heartbeat while backend is silent')
                raise KeyboardInterrupt()
        self.assertFalse(progress.worker.is_alive())
        self.assertIsNone(run.progress)
        self.assertTrue(stream.getvalue().endswith('\r\033[K'))

    def test_never_and_dumb_terminals_do_not_start_status_threads(self):
        for mode, term in (('never', 'xterm'), ('auto', 'dumb')):
            stream, run = TTY(), harness.Run()
            with self.subTest(mode=mode, term=term), mock.patch.dict(os.environ, {'TERM': term}):
                with harness.TerminalProgress(run, mode=mode, stream=stream) as progress:
                    run.record({'step': 'review', 'attempt': 1, 'status': 'started'})
                self.assertIsNone(progress.worker)
                self.assertEqual(stream.getvalue(), '')

    def test_summary_can_be_forced_to_json_in_a_terminal_or_text_when_redirected(self):
        run = harness.Run()
        run.reserve()
        run.record({'status': 'ok', 'usage': USAGE})
        for stream, style in ((TTY(), 'json'), (io.StringIO(), 'text'), (TTY(), 'auto')):
            with self.subTest(style=style), mock.patch('sys.stderr', stream), \
                    mock.patch.dict(os.environ, {'TERM': 'xterm'}):
                harness.print_summary(run.summary(elapsed_s=2), style)
            if style == 'json':
                self.assertEqual(json.loads(stream.getvalue().removeprefix('harness: '))['calls'], 1)
            else:
                self.assertIn('1 call | 2.00s | 160 total tokens', stream.getvalue())
                self.assertIn('[100 input + 40 cache read + 0 cache write + 20 output]', stream.getvalue())
                self.assertIn('10.0 output tok/s (wall time)', stream.getvalue())
                self.assertIn('1 call unpriced', stream.getvalue())

    def test_text_summary_marks_partial_totals_for_missing_usage_and_inflight_calls(self):
        for state in ('unreported', 'unfinished'):
            with self.subTest(state=state):
                run = harness.Run()
                run.reserve()
                run.record({'status': 'ok', 'usage': USAGE})
                run.reserve()
                if state == 'unreported':
                    run.record({'status': 'error'})
                with mock.patch('sys.stderr', new_callable=io.StringIO) as err:
                    harness.print_summary(run.summary(elapsed_s=2), 'text')
                self.assertIn('160 total tokens (partial;', err.getvalue())
                self.assertIn('output tok/s unknown', err.getvalue())

    def test_cli_interrupt_returns_130_and_preserves_unknown_usage(self):
        with tempfile.TemporaryDirectory() as temp:
            material, trace = Path(temp) / 'input', Path(temp) / 'trace'
            material.write_text('fixture')
            def interrupt(plan, material, backend, run, **kwargs):
                run.reserve()
                run.record({'step': 'review', 'attempt': 1, 'status': 'error'})
                raise KeyboardInterrupt()
            with mock.patch.object(harness, 'run_plan', side_effect=interrupt), \
                    mock.patch('sys.stderr', new_callable=io.StringIO) as err:
                code = harness.main(['review', '--input', str(material), '--trace', str(trace)])
            self.assertEqual(code, 130)
            self.assertIn('review interrupted', err.getvalue())
            summary = json.loads((trace / 'summary.json').read_text())
            self.assertEqual(summary['status'], 'interrupted')
            self.assertIsNone(summary['output_tokens_per_second'])

    def test_failed_cli_run_writes_usage_summary_and_leaves_stdout_clean(self):
        with tempfile.TemporaryDirectory() as temp:
            material, trace = Path(temp) / 'input', Path(temp) / 'trace'
            material.write_text('fixture')
            def fail(plan, material, backend, run, **kwargs):
                run.reserve()
                run.record({'step': 'review', 'attempt': 1, 'status': 'error'})
                raise harness.CallError('fixture failure')
            with mock.patch.object(harness, 'run_plan', side_effect=fail), \
                    mock.patch('sys.stderr', new_callable=io.StringIO) as err, \
                    mock.patch('sys.stdout', new_callable=io.StringIO) as out:
                code = harness.main(['review', '--input', str(material), '--trace', str(trace),
                                     '--progress', 'never', '--summary-format', 'text'])
            self.assertEqual(code, 2)
            self.assertEqual(out.getvalue(), '')
            self.assertIn('output tok/s unknown', err.getvalue())
            summary = json.loads((trace / 'summary.json').read_text())
            self.assertEqual(summary['calls'], 1)
            self.assertIsNone(summary['output_tokens_per_second'])
            self.assertGreater(summary['elapsed_s'], 0)


if __name__ == '__main__':
    unittest.main()
