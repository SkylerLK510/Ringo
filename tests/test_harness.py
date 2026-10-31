"""Offline checks of harness.py and pipelines.py: fake backends only, no model calls."""
import io
import json
import os
import shlex
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import backends  # noqa: E402
import harness  # noqa: E402
import pipelines as pl  # noqa: E402
from tests.support import POSIX_MODES, python_cli  # noqa: E402

REVIEW = 'What this change does: x.\n\n### 1. Bug one (`a.py:1`)\n- Code: `a`\n- Problem: p1. Side claim here.\n\n' \
         '### 2. Bug two (`a.py:2`)\n- Code: `b`\n- Problem: p2.\n\n### Not checked\n- runtime'
USAGE = {'input': 2, 'cache_write': 100, 'cache_read': 0, 'output': 10, 'cost_usd': 0.01}
B = {'name': 'x', 'argv': ['x'], 'style': 'plain'}


def plan(passes, kind='review', d=None):
    d = d or tempfile.mkdtemp()
    for p in passes:
        Path(d, p['prompt']).write_text(f"INSTRUCTION {p['id']}", encoding='utf-8')
    return pl.validate({'version': 1, 'kind': kind, 'role': 'ROLE', 'passes': passes}, d)


def scripted(replies):
    """A call_fn that answers by pass instruction, records (system, message) and is thread-safe."""
    seen, lock = [], threading.Lock()

    def fn(backend, system, message, timeout_s, model=None):
        with lock:
            seen.append((system, message))
        for key, reply in replies.items():
            if key in message:
                out = reply(message) if callable(reply) else reply
                return (out.pop(0) if isinstance(out, list) else out), dict(USAGE)
        raise AssertionError(f'no scripted reply for {message[:60]!r}')
    return fn, seen


class CallTest(unittest.TestCase):
    def fake_claude(self, body):
        return python_cli(Path(tempfile.mkdtemp(), 'claude'), body)

    def test_claude_gets_material_in_a_system_prompt_file_and_reports_usage(self):
        exe = self.fake_claude(
            'import sys, json\na = sys.argv\nf = a[a.index("--system-prompt-file") + 1]\n'
            'assert a[a.index("--output-format") + 1] == "json"\n'
            'print(json.dumps({"result": open(f).read() + "|" + sys.stdin.read(), "total_cost_usd": 0.02,\n'
            '  "usage": {"input_tokens": 3, "cache_creation_input_tokens": 7, "cache_read_input_tokens": 90, "output_tokens": 4}}))\n')
        text, usage = harness.call({'name': 'c', 'argv': [*exe, '-p'], 'style': 'claude'}, 'SYSTEM+MATERIAL', 'INSTRUCTION', 30)
        self.assertEqual(text, 'SYSTEM+MATERIAL|INSTRUCTION')
        self.assertEqual(usage, {'input': 3, 'cache_write': 7, 'cache_read': 90, 'output': 4, 'cost_usd': 0.02, 'models': []})

    def test_the_claude_preset_runs_on_the_login_not_an_api_key(self):
        exe = self.fake_claude('import json, os, sys\nsys.stdin.read()\nkeys = [k for k in ("ANTHROPIC_API_KEY", '
                               '"ANTHROPIC_BASE_URL", "CLAUDE_CODE_USE_BEDROCK", "KEEP") if k in os.environ]\n'
                               'print(json.dumps({"result": " ".join(keys)}))\n')
        env = {'ANTHROPIC_API_KEY': 'x', 'ANTHROPIC_BASE_URL': 'http://x', 'CLAUDE_CODE_USE_BEDROCK': '1', 'KEEP': '1'}
        with mock.patch.dict(os.environ, env):
            preset = {**harness.backend('claude'), 'argv': [*exe, '-p']}
            self.assertEqual(harness.call(preset, 's', 'm', 30)[0], 'KEEP')
            custom = {**harness.backend('claude:claude -p'), 'argv': [*exe, '-p']}
            self.assertEqual(harness.call(custom, 's', 'm', 30)[0], 'ANTHROPIC_API_KEY ANTHROPIC_BASE_URL CLAUDE_CODE_USE_BEDROCK KEEP')

    def test_claude_errors_and_non_json_raise(self):
        bad = self.fake_claude('import json\nprint(json.dumps({"is_error": True, "result": "limit reached"}))\n')
        with self.assertRaisesRegex(backends.JudgeError, 'limit reached'):
            harness.call({'name': 'c', 'argv': bad, 'style': 'claude'}, 's', 'm', 30)
        junk = self.fake_claude('print("not json")\n')
        with self.assertRaisesRegex(backends.JudgeError, 'did not return JSON') as caught:
            harness.call({'name': 'c', 'argv': junk, 'style': 'claude'}, 's', 'm', 30)
        self.assertEqual(caught.exception.raw.strip(), 'not json')  # kept for the trace
        with self.assertRaises(harness.CallError) as caught:
            harness.call({'name': 'c', 'argv': bad, 'style': 'claude'}, 's', 'm', 30)
        self.assertIn('limit reached', caught.exception.raw)

    def test_other_backends_get_system_then_message_on_stdin(self):
        argv = shlex.split(shlex.join([sys.executable, '-c', 'import sys; print(sys.stdin.read())']))
        text, usage = harness.call({'name': 'py', 'argv': argv, 'style': 'plain'}, 'SYS', 'MSG', 30)
        self.assertEqual((text.strip(), usage), ('SYS\n\nMSG', None))
        with self.assertRaisesRegex(ValueError, 'claude-style'):
            harness.call({'name': 'py', 'argv': argv, 'style': 'plain'}, 'SYS', 'MSG', 30, model='haiku')

    def test_windows_command_strings_keep_backslashes_and_split_like_posix(self):
        cases = {r'C:\Tools\cli.exe -p': [r'C:\Tools\cli.exe', '-p'],
                 r'"C:\Program Files\cli.exe" --x "a b" c#d': [r'C:\Program Files\cli.exe', '--x', 'a b', 'c#d'],
                 shlex.join([r'C:\py\python.exe', '-c', "print('a # b')"]): [r'C:\py\python.exe', '-c', "print('a # b')"]}
        with mock.patch.object(harness.os, 'name', 'nt'):
            for command, argv in cases.items():
                self.assertEqual(harness.split_command(command), argv)
            for preset in backends.JUDGE_PRESETS.values():  # POSIX-quoted, without backslashes: split alike
                self.assertEqual(harness.split_command(preset), shlex.split(preset))

    def test_backend_styles_are_declared_not_guessed(self):
        self.assertEqual(harness.backend('claude'), {'name': 'claude', 'argv': shlex.split(backends.JUDGE_PRESETS['claude']),
                                                     'style': 'claude', 'subscription_auth': True})
        self.assertNotIn('subscription_auth', harness.backend('claude:claude -p'))  # a command you wrote is yours
        self.assertEqual(harness.backend('codex')['style'], 'codex')
        self.assertEqual(harness.backend('codex --search exec --json -')['style'], 'plain')  # no name sniffing
        self.assertEqual(harness.backend('codex-chatgpt')['style'], 'codex')
        self.assertEqual(harness.backend('ollama:qwen3')['argv'], ['ollama', 'run', 'qwen3'])
        self.assertEqual(harness.backend('claude:my-wrapper -p')['style'], 'claude')
        self.assertEqual(harness.backend('/usr/local/bin/claude -p')['style'], 'plain')  # no name sniffing
        for bad in ('  ', 'ollama:', 'claude:'):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                harness.backend(bad)
        self.assertEqual(harness.with_model(['c', '--model', 'sonnet', '-p'], 'haiku'), ['c', '--model', 'haiku', '-p'])
        self.assertEqual(harness.with_model(['c', '-p'], 'haiku'), ['c', '-p', '--model', 'haiku'])

    def test_malformed_usage_preserves_the_paid_price(self):
        usage = harness.claude_usage({'total_cost_usd': 0.12, 'modelUsage': 42,
                                     'usage': {'input_tokens': 'bad', 'output_tokens': -2,
                                               'cache_creation_input_tokens': True}})
        run = harness.Run(max_cost=0.1)
        run.reserve()
        run.record({'status': 'ok', 'usage': usage})
        self.assertEqual(run.summary()['cost_usd'], 0.12)
        self.assertEqual(run.summary()['input'], 0)
        self.assertEqual(run.summary()['output'], 0)
        self.assertEqual(run.summary()['cache_write'], 0)
        with self.assertRaises(harness.BudgetExceeded):
            run.reserve()

    def test_explicit_admission_refusal_is_not_charged_by_review_or_design(self):
        for kind in ('review', 'design'):
            p = pl.load_plan(None, kind)
            run = harness.Run(unpriced_call_usd=0.5)
            def refuse(*args):
                raise harness.AdmissionRefused('not started')
            with self.subTest(kind=kind), self.assertRaises(harness.AdmissionRefused):
                harness.run_plan(p, 'material', B, run, call_fn=refuse)
            self.assertEqual(run.summary()['calls'], 0)
            self.assertEqual(run.summary()['cost_usd'], 0)
            self.assertEqual(run.summary()['unpriced_calls'], 0)
            self.assertEqual(run.summary()['admission_refusals'], 1)

    def test_interrupted_review_or_design_keeps_cost_uncertainty(self):
        for kind in ('review', 'design'):
            p = pl.load_plan(None, kind)
            run = harness.Run()
            def interrupt(*args):
                raise KeyboardInterrupt()
            with self.subTest(kind=kind), self.assertRaises(KeyboardInterrupt):
                harness.run_plan(p, 'material', B, run, call_fn=interrupt)
            self.assertEqual(run.summary()['calls'], 1)
            self.assertEqual(run.summary()['unpriced_calls'], 1)

    @staticmethod
    def codex_events():
        return '\n'.join(json.dumps(x) for x in [
            {'type': 'item.completed', 'item': {'type': 'agent_message', 'text': 'No defects found.'}},
            {'type': 'turn.completed', 'usage': {'input_tokens': 100, 'cached_input_tokens': 70,
                                                'output_tokens': 12}}])

    def test_chatgpt_backend_forces_native_auth_disables_nested_tools_and_records_tokens(self):
        backend = harness.backend('codex-chatgpt')
        self.assertIn('forced_login_method="chatgpt"', backend['argv'])
        self.assertIn('model_provider="openai"', backend['argv'])
        self.assertIn('--json', backend['argv'])
        self.assertIn('skip_host_skill_discovery', backend['argv'])
        with mock.patch.object(backends, 'run_isolated', return_value=self.codex_events()) as invoke:
            text, usage = harness.call(backend, 'SYS', 'MSG', 30)
        invoke.assert_called_once_with(backend['argv'], 'SYS\n\nMSG', 30, codex_home=True, subscription_auth=True,
                                       lean=True, watch=None)
        self.assertEqual(text, 'No defects found.')
        self.assertEqual(usage, {'input': 30, 'cache_read': 70, 'cache_write': 0, 'output': 12, 'cost_usd': None,
                                 'items': {'agent_message': 1}, 'warnings': []})

    @staticmethod
    def event(item_type, **fields):
        return json.dumps({'type': 'item.completed', 'item': {'type': item_type, **fields}})

    def test_codex_warnings_and_progress_events_are_tolerated_and_recorded(self):
        noise = [self.event('error', message='skip_host_skill_discovery is unstable'), self.event('reasoning', text='r'),
                 self.event('todo_list', items=[]), json.dumps({'type': 'item.started', 'item': {'type': 'reasoning'}}),
                 json.dumps({'type': 'item.updated', 'item': {'type': 'todo_list'}}),
                 json.dumps({'type': 'thread.started'}), json.dumps({'type': 'turn.started'})]
        text, usage = harness.codex_reply('\n'.join(noise) + '\n' + self.codex_events())
        self.assertEqual(text, 'No defects found.')
        self.assertEqual(usage['output'], 12)
        self.assertEqual(usage['warnings'], ['skip_host_skill_discovery is unstable'])
        self.assertEqual(usage['items'], {'error': 1, 'reasoning': 1, 'todo_list': 1, 'agent_message': 1})

    def test_codex_rejection_names_the_reason_and_item_counts_and_keeps_raw(self):
        cases = {'undeclared tool item command_execution': self.event('command_execution'),
                 'undeclared tool item mcp_tool_call': self.event('mcp_tool_call'),
                 'undeclared tool item web_search': self.event('web_search'),
                 'undeclared tool item file_change': self.event('file_change'),
                 'turn.failed: boom': json.dumps({'type': 'turn.failed', 'error': 'boom'}),
                 'error: Reconnecting': json.dumps({'type': 'error', 'message': 'Reconnecting'}),
                 '2 turn.completed events': json.dumps({'type': 'turn.completed', 'usage': {}}),
                 'not a JSON event': 'not JSON'}
        for reason, tail in cases.items():
            raw = self.codex_events() + '\n' + tail
            with self.subTest(reason=reason), self.assertRaises(harness.CallError) as caught:
                harness.codex_reply(raw, 'codex-chatgpt')
            self.assertIn(reason, str(caught.exception))
            self.assertIn('agent_message=1', str(caught.exception))
            self.assertEqual(caught.exception.raw, raw)
            self.assertEqual(caught.exception.usage['output'], 12)
        with self.assertRaisesRegex(harness.CallError, 'no final agent message'):
            harness.codex_reply(json.dumps({'type': 'turn.completed', 'usage': {}}))

    def test_codex_backends_are_declared_with_their_tools_and_auth(self):
        plain = harness.backend('codex')
        self.assertIn('--json', plain['argv'])
        self.assertEqual((plain['subscription_auth'], plain['tools']), (False, ['command_execution']))
        chatgpt = harness.backend('codex-chatgpt')
        self.assertEqual((chatgpt['subscription_auth'], chatgpt['tools']), (True, []))
        custom = harness.backend('codex:codex --search exec --skip-git-repo-check --json -')
        self.assertEqual((custom['style'], custom['argv'][0], custom['subscription_auth'], custom['tools']),
                         ('codex', 'codex', False, ['web_search']))
        self.assertEqual(harness.backend('codex:codex exec --json -')['tools'], [])
        for bad in ('codex:', 'codex:codex exec -'):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                harness.backend(bad)

    def test_codex_prefix_parses_tokens_and_allows_only_declared_search(self):
        events = self.event('web_search', query='q') + '\n' + self.codex_events()
        searching = harness.backend('codex:codex --search exec --json -')
        with mock.patch.object(backends, 'run_isolated', return_value=events) as invoke:
            text, usage = harness.call(searching, 'S', 'M', 30)
        invoke.assert_called_once_with(searching['argv'], 'S\n\nM', 30, codex_home=True, subscription_auth=False,
                                       lean=False, watch=None)
        self.assertEqual((text, usage['input'], usage['cache_read'], usage['output']), ('No defects found.', 30, 70, 12))
        for spec in ('codex:codex exec --json -', 'codex-chatgpt'):
            with self.subTest(spec=spec), mock.patch.object(backends, 'run_isolated', return_value=events), \
                    self.assertRaisesRegex(harness.CallError, f'{spec} turn rejected: undeclared tool item web_search'):
                harness.call(harness.backend(spec), 'S', 'M', 30)
        shell = self.event('command_execution') + '\n' + self.codex_events()
        with mock.patch.object(backends, 'run_isolated', return_value=shell), self.assertRaises(harness.CallError):
            harness.call(searching, 'S', 'M', 30)
        with mock.patch.object(backends, 'run_isolated', return_value=shell):  # the codex preset keeps its shell
            self.assertEqual(harness.call(harness.backend('codex'), 'S', 'M', 30)[0], 'No defects found.')

    def test_hand_built_codex_backend_keeps_key_stripping_and_private_home(self):
        b = {'name': 'codex-chatgpt+model', 'argv': ['codex', 'exec', '--json', '-'], 'style': 'codex'}
        with mock.patch.object(backends, 'run_isolated', return_value=self.codex_events()) as invoke:
            harness.call(b, 'S', 'M', 30)
        invoke.assert_called_once_with(b['argv'], 'S\n\nM', 30, codex_home=True, subscription_auth=True,
                                       lean=False, watch=None)

    def test_native_turn_failures_preserve_reported_tokens_without_becoming_success(self):
        for tail in ('not JSON', json.dumps({'type': 'turn.failed'}),
                     json.dumps({'type': 'item.completed', 'item': {'type': 'command_execution'}})):
            with self.subTest(tail=tail), self.assertRaises(harness.CallError) as caught:
                harness.codex_reply(self.codex_events() + '\n' + tail)
            self.assertEqual(caught.exception.usage['output'], 12)
        with self.assertRaises(harness.CallError):
            harness.codex_reply('{"type":"item.completed","item":{"type":"agent_message","text":"draft"}}')

    def test_native_process_failure_preserves_completed_usage(self):
        error = backends.JudgeError('codex exited with 1')
        error.stdout = self.codex_events()
        with mock.patch.object(backends, 'run_isolated', side_effect=error), \
                self.assertRaises(harness.CallError) as caught:
            harness.call(harness.backend('codex-chatgpt'), 's', 'm', 1)
        self.assertEqual(caught.exception.usage['input'], 30)
        self.assertEqual(caught.exception.raw, self.codex_events())

    def test_codex_timeout_keeps_partial_usage_and_raw_events_in_the_trace(self):
        # a stand-in Codex child: prints its events, then hangs past the timeout
        events = self.codex_events()
        child = f'import sys, time; sys.stdout.write({events!r} + "\\n"); sys.stdout.flush(); time.sleep(10)'
        backend = {**harness.backend('codex-chatgpt'), 'argv': [sys.executable, '-c', child]}
        with tempfile.TemporaryDirectory() as d, \
                mock.patch.dict(os.environ, {'HOME': d, 'CODEX_HOME': str(Path(d, 'codex'))}), \
                mock.patch.object(harness, 'HARD_FLOOR_S', 0), mock.patch('sys.stderr', new_callable=io.StringIO):
            run = harness.Run(Path(d, 't'))
            with self.assertRaises(subprocess.TimeoutExpired) as caught:  # soft at 1s, stopped at 2s
                harness.run_plan(pl.load_plan(None, 'review'), 'M', backend, run, timeout_s=1,
                                 call_fn=harness.call)
            self.assertTrue(caught.exception.limit)
            rows = [json.loads(x) for x in Path(d, 't', 'trace.jsonl').read_text().splitlines()]
            errors = [r for r in rows if r['status'] == 'error' and r['step'] == 'review']
            self.assertEqual(len(errors), 1)  # the whole window was used: no retry after the hard limit
            self.assertEqual((errors[0]['stopped'], errors[0]['soft_s'], errors[0]['limit_s']), ('limit', 1, 2))
            self.assertEqual(errors[0]['usage']['input'], 30)
            raw = Path(d, 't', 'steps', f"{errors[0]['step']}.1.raw.txt").read_text()
            self.assertIn('No defects found.', raw)

    def test_codex_empty_builtin_resource_listings_are_tolerated_but_nothing_else(self):
        def listing(tool, text, server='codex'):
            return json.dumps({'type': 'item.completed', 'item': {
                'type': 'mcp_tool_call', 'server': server, 'tool': tool, 'arguments': {}, 'error': None,
                'status': 'completed', 'result': {'content': [{'type': 'text', 'text': text}], 'structured_content': None}}})
        done = [json.dumps({'type': 'item.completed', 'item': {'type': 'agent_message', 'text': 'review'}}),
                json.dumps({'type': 'turn.completed', 'usage': {'input_tokens': 5, 'output_tokens': 2}})]
        ok = [listing('list_mcp_resources', '{"resources":[]}'),
              listing('list_mcp_resource_templates', '{"resourceTemplates":[]}')]
        text, usage = harness.codex_reply('\n'.join(ok + done))
        self.assertEqual((text, usage['items']['mcp_tool_call']), ('review', 2))
        for bad in (listing('list_mcp_resources', '{"resources":[{"uri":"x"}]}'),
                    listing('list_mcp_resources', '{"resources":[]}', server='other'),
                    listing('read_mcp_resource', '{"resources":[]}')):
            with self.subTest(bad=bad), self.assertRaisesRegex(harness.CallError, 'undeclared tool item mcp_tool_call'):
                harness.codex_reply('\n'.join([bad, *done]))

    def test_codex_failure_without_a_completed_turn_counts_as_untokened_not_zero(self):
        cases = {'turn.failed': json.dumps({'type': 'turn.failed', 'error': 'usage limit'}),
                 'stream cut': json.dumps({'type': 'thread.started'})}
        for name, raw in cases.items():
            with self.subTest(name=name), self.assertRaises(harness.CallError) as caught:
                harness.codex_reply(raw)
            self.assertNotIn('input', caught.exception.usage)
        error = backends.JudgeError('codex exited with 1')
        error.stdout = ''
        with mock.patch.object(backends, 'run_isolated', side_effect=error), \
                self.assertRaises(harness.CallError) as caught:
            harness.call(harness.backend('codex-chatgpt'), 's', 'm', 1)
        done = json.dumps({'type': 'item.completed', 'item': {'type': 'agent_message', 'text': 'ok'}})
        _, usage = harness.codex_reply(done + '\n' + json.dumps({'type': 'turn.completed'}))
        self.assertNotIn('input', usage)  # a completed turn that reports no usage is unknown, not zero
        run = harness.Run()
        run.record({'status': 'error', 'usage': caught.exception.usage})
        run.record({'status': 'ok', 'usage': {'cost_usd': None, 'items': {}, 'warnings': []}})
        summary = run.summary()
        self.assertEqual((summary['untokened_calls'], summary['input'], summary['output']), (2, 0, 0))

    def test_model_override_replaces_attached_and_duplicate_options(self):
        self.assertEqual(harness.with_model(['c', '--model=large', '-p', '--model', 'other'], 'small'),
                         ['c', '--model', 'small', '-p'])
        self.assertEqual(harness.with_model(['c', '--model'], 'small'), ['c', '--model', 'small'])


class RunTest(unittest.TestCase):
    def test_two_writers_cannot_claim_the_same_empty_trace(self):
        with tempfile.TemporaryDirectory() as d:
            trace, barrier, results = Path(d), threading.Barrier(2), []
            real_iterdir = Path.iterdir

            def simultaneous_empty_check(path):
                entries = list(real_iterdir(path))
                if path == trace:
                    barrier.wait(timeout=5)
                return iter(entries)

            def claim():
                try:
                    results.append(harness.Run(trace))
                except (OSError, ValueError) as exc:
                    results.append(exc)
            with mock.patch.object(Path, 'iterdir', simultaneous_empty_check):
                workers = [threading.Thread(target=claim) for _ in range(2)]
                for worker in workers:
                    worker.start()
                for worker in workers:
                    worker.join(timeout=5)
            self.assertEqual(sum(isinstance(result, harness.Run) for result in results), 1)
            self.assertEqual(len(results), 2)

    def test_trace_reuse_is_refused_before_overwriting_an_earlier_run(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d, 'material.txt')
            path.write_text('earlier material')
            with self.assertRaisesRegex(ValueError, 'new or empty'):
                harness.Run(d)
            self.assertEqual(path.read_text(), 'earlier material')

    def test_there_is_no_call_cap(self):
        with self.assertRaises(TypeError):
            harness.Run(max_calls=1)
        run = harness.Run()
        for _ in range(500):  # the plan bounds the calls, the accounting only counts them
            run.reserve()
        self.assertEqual(run.summary()['calls'], 500)
        import contextlib, io
        with tempfile.TemporaryDirectory() as d:
            path = Path(d, 'm.txt')
            path.write_text('M')
            err = io.StringIO()
            with contextlib.redirect_stderr(err), self.assertRaises(SystemExit) as caught:
                harness.main(['review', '--input', str(path), '--max-calls', '1'])
            self.assertEqual(caught.exception.code, 2)
            self.assertIn('--max-calls was removed', err.getvalue())

    def test_blocked_retry_reports_the_first_attempts_error_and_keeps_raw_output(self):
        def reject(*args):
            raise harness.CallError('codex-chatgpt turn rejected: no final agent message', dict(USAGE), 'R' * 1_500_000)
        with tempfile.TemporaryDirectory() as d:
            run = harness.Run(Path(d, 't'), max_cost=0.01, unpriced_call_usd=0.5)  # the first attempt spends it all
            with self.assertRaises(harness.BudgetExceeded) as caught:  # the type callers catch is unchanged
                harness.run_plan(pl.load_plan(None, 'review'), 'M', B, run, call_fn=reject)
            self.assertIn('cost cap reached', str(caught.exception))
            self.assertIn('retry not started after attempt 1 failed: CallError: codex-chatgpt turn rejected: '
                          'no final agent message', str(caught.exception))
            self.assertIsInstance(caught.exception.__cause__, harness.CallError)
            rows = [json.loads(x) for x in Path(d, 't', 'trace.jsonl').read_text().splitlines()]
            self.assertEqual([r['status'] for r in rows], ['started', 'error'])
            raw = Path(d, 't', 'steps', f"{rows[1]['step']}.1.raw.txt")
            self.assertEqual(raw.stat().st_size, harness.RAW_LIMIT)

    def test_a_failed_call_whose_retry_cannot_start_names_its_error(self):
        def timeout(*args):
            raise backends.JudgeError('codex exited with 1: stream disconnected')
        run = harness.Run(max_cost=1.0)  # no fallback price: the failed attempt's unknown cost blocks the retry
        with self.assertRaisesRegex(harness.BudgetExceeded, 'attempt 1 failed: JudgeError: codex exited with 1'):
            harness.run_plan(pl.load_plan(None, 'design'), 'M', B, run, call_fn=timeout)

    def test_unknown_tokens_are_counted_not_reported_as_zero(self):
        run = harness.Run()
        for rec in ({'status': 'ok', 'usage': None}, {'status': 'error'}, {'status': 'malformed', 'usage': dict(USAGE)},
                    {'status': 'started'}, {'status': 'skipped'}, {'status': 'fatal', 'error': 'x'}):
            run.record(rec)
        summary = run.summary()
        self.assertEqual((summary['untokened_calls'], summary['unpriced_calls']), (2, 2))
        self.assertEqual((summary['input'], summary['output']), (2, 10))  # sums stay ints
        fn = lambda *args: (REVIEW, None)  # noqa: E731  a plain backend reports no usage
        run = harness.Run()
        harness.run_plan(pl.load_plan(None, 'review'), 'M', B, run, call_fn=fn)
        self.assertEqual((run.summary()['untokened_calls'], run.summary()['input']), (1, 0))

    def test_a_conditional_reader_can_be_skipped(self):
        p = plan([{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'},
                  {'id': 'audit', 'prompt': 'audit.md', 'reply': 'issues'},
                  {'id': 'b', 'prompt': 'b.md', 'reply': 'findings', 'when': 'audit'},
                  {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}])
        fn, seen = scripted({'INSTRUCTION audit': 'NONE', 'INSTRUCTION a': REVIEW,
                             'INSTRUCTION k': 'a.1 KEEP\na.2 KEEP'})
        text, stats = harness.run_plan(p, 'M', B, harness.Run(), call_fn=fn)
        self.assertEqual(stats['kept'], 2)
        self.assertIn('Bug two', text)
        self.assertFalse(any('INSTRUCTION b' in m for _, m in seen))

    def test_repair_cannot_change_an_already_checked_finding(self):
        p = plan([{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'},
                  {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}])
        fn, _ = scripted({'INSTRUCTION a': REVIEW,
                          'still need a verdict': 'a.2 KEEP\na.1 REFUTED\na.1 CUT "p1."',
                          'INSTRUCTION k': 'a.1 KEEP'})
        text, stats = harness.run_plan(p, 'M', B, harness.Run(), call_fn=fn)
        self.assertEqual(stats['kept'], 2)
        self.assertIn('p1.', text)

    def test_one_call_review_is_verbatim_with_cost(self):
        fn, seen = scripted({'Review the change': REVIEW})
        run = harness.Run()
        text, stats = harness.run_plan(pl.load_plan(None, 'review'), 'THE CODE', B, run, call_fn=fn)
        self.assertEqual(text, REVIEW.strip())
        self.assertEqual((run.summary()['calls'], run.summary()['cost_usd']), (1, 0.01))
        system, message = seen[0]
        self.assertIn('THE CODE', system)
        self.assertNotIn('THE CODE', message)

    def test_every_pass_shares_one_system_prompt_and_the_first_runs_alone(self):
        order, lock = [], threading.Lock()
        p = plan([{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'}, {'id': 'b', 'prompt': 'b.md', 'reply': 'findings'},
                  {'id': 'c', 'prompt': 'c.md', 'reply': 'findings'}, {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}])

        def fn(backend, system, message, timeout_s, model=None):
            with lock:
                order.append((message.split()[1], system))
            if 'INSTRUCTION k' in message:
                return '\n'.join(f'{x}.{n} KEEP - q' for x in 'abc' for n in (1, 2)), None
            return REVIEW, None
        harness.run_plan(p, 'MAT', B, harness.Run(), call_fn=fn)
        self.assertEqual(order[0][0], 'a')                       # the cache writer goes first
        self.assertEqual(len({s for _, s in order}), 1)          # one shared, cacheable system prompt
        self.assertEqual(order[-1][0], 'k')                      # the checker waits for every reader

    def test_wrong_shape_is_asked_once_more_then_fails(self):
        p = plan([{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'}, {'id': 'b', 'prompt': 'b.md', 'reply': 'findings'},
                  {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}])
        fn, _ = scripted({'INSTRUCTION a': ['prose with no headings', REVIEW], 'INSTRUCTION b': 'No defects found.',
                          'INSTRUCTION k': 'a.1 KEEP - q\na.2 REFUTED - q'})
        run = harness.Run()
        text, stats = harness.run_plan(p, 'M', B, run, call_fn=fn)
        self.assertEqual((stats['kept'], run.summary()['calls']), (1, 4))
        fn2, _ = scripted({'INSTRUCTION a': 'never any headings', 'INSTRUCTION b': REVIEW, 'INSTRUCTION k': 'b.1 KEEP - q'})
        with self.assertRaisesRegex(ValueError, 'no finding headings'):
            harness.run_plan(p, 'M', B, harness.Run(), call_fn=fn2)

    def test_a_review_makes_exactly_its_structural_number_of_calls(self):
        for kind in ('review', 'design'):  # the built-in plans: one call
            run = harness.Run()
            harness.run_plan(pl.load_plan(None, kind), 'M', B, run, call_fn=lambda *a: (REVIEW, dict(USAGE)))
            self.assertEqual(run.summary()['calls'], 1)
        p = plan([{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'}, {'id': 'b', 'prompt': 'b.md', 'reply': 'findings'},
                  {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts', 'batch': 1}])

        def verdicts(message):  # every batch answered, except b.2, which the repair call then answers
            ids = message.split('in this batch: ', 1)[1].split('.\n', 1)[0].split(', ')
            return '\n'.join(f'{i} KEEP - q' for i in ids if i != 'b.2') or 'a.1 KEEP - q'
        fn, seen = scripted({'still need a verdict': 'b.2 KEEP - q', 'INSTRUCTION a': ['prose, no headings', REVIEW],
                             'INSTRUCTION b': REVIEW, 'INSTRUCTION k': verdicts})
        run = harness.Run()
        text, stats = harness.run_plan(p, 'M', B, run, call_fn=fn)
        # 2 readers + 4 checker batches (2 findings each, batch 1) + 1 retry of a's wrong shape + 1 repair = 8
        self.assertEqual((run.summary()['calls'], len(seen), stats['kept']), (8, 8, 4))

    def test_a_command_that_cannot_start_is_not_retried(self):
        calls = []

        def fn(backend, system, message, timeout_s, model=None):
            calls.append(1)
            raise FileNotFoundError('no such binary')
        with self.assertRaises(FileNotFoundError):
            harness.run_plan(pl.load_plan(None, 'review'), 'M', B, harness.Run(), call_fn=fn)
        self.assertEqual(len(calls), 1)

    def test_a_cost_cap_stops_new_calls(self):
        run = harness.Run(max_cost=0.01)
        run.reserve()
        run.record({'step': 's', 'attempt': 1, 'status': 'ok', 'usage': dict(USAGE)})
        with self.assertRaisesRegex(harness.BudgetExceeded, 'cost cap'):
            run.reserve()

    def test_paid_malformed_replies_are_charged_and_unknown_costs_stop_a_cost_cap(self):
        p = plan([{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'}, {'id': 'b', 'prompt': 'b.md', 'reply': 'findings'},
                  {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}])
        fn, _ = scripted({'INSTRUCTION a': ['no headings at all', REVIEW], 'INSTRUCTION b': REVIEW,
                          'INSTRUCTION k': '\n'.join(f'{x}.{n} KEEP' for x in 'ab' for n in (1, 2))})
        run = harness.Run()
        harness.run_plan(p, 'M', B, run, call_fn=fn)
        self.assertAlmostEqual(run.summary()['cost_usd'], 0.04)        # 4 replies, the malformed one included
        unpriced = harness.Run(max_cost=1.0)
        unpriced.reserve()
        unpriced.record({'step': 's', 'attempt': 1, 'status': 'ok', 'usage': None})
        with self.assertRaisesRegex(harness.BudgetExceeded, 'unknown'):
            unpriced.reserve()

    def test_failed_calls_keep_their_reported_cost_and_bad_prices_count_as_unknown(self):
        exe = python_cli(Path(tempfile.mkdtemp(), 'claude'), 'import json, sys\nprint(json.dumps({"is_error": True, '
                         '"result": "boom", "total_cost_usd": 0.02, "usage": {"input_tokens": 50}}))\nsys.exit(1)\n')
        with self.assertRaises(harness.CallError) as cm:
            harness.call({'name': 'c', 'argv': exe, 'style': 'claude'}, 's', 'm', 30)
        self.assertEqual((cm.exception.usage['cost_usd'], cm.exception.usage['input']), (0.02, 50))
        for bad in (-0.01, float('nan'), True, '0.01'):
            self.assertIsNone(harness.valid_price(bad))
        run = harness.Run(max_cost=0.001)
        run.reserve()
        run.record({'step': 's', 'attempt': 1, 'status': 'ok', 'usage': {'cost_usd': -5.0}})
        with self.assertRaisesRegex(harness.BudgetExceeded, 'unknown'):
            run.reserve()

    def test_an_unexpected_adapter_error_still_settles_the_attempt_and_limits_are_validated(self):
        d = Path(tempfile.mkdtemp()) / 'trace'

        def broken(backend, system, message, timeout_s, model=None):
            raise AttributeError('usage was a list')
        run = harness.Run(d)
        with self.assertRaises(AttributeError):
            harness.run_plan(pl.load_plan(None, 'review'), 'M', B, run, call_fn=broken)
        statuses = [json.loads(line)['status'] for line in (d / 'trace.jsonl').read_text().splitlines()]
        self.assertEqual(statuses, ['started', 'error'])                   # settled, not left pending
        self.assertEqual(run.summary()['unpriced_calls'], 1)               # and its cost is unknown
        self.assertEqual(harness.claude_usage({'usage': [1], 'total_cost_usd': 0.02})['cost_usd'], 0.02)
        for bad in (float('nan'), -0.01):
            with self.assertRaises(ValueError):
                harness.Run(max_cost=bad)
            with self.assertRaises(ValueError):
                harness.Run(unpriced_call_usd=bad)
        material = Path(tempfile.mkdtemp(), 'm.txt')
        material.write_text('x')
        with mock.patch('sys.stderr'):
            self.assertEqual(harness.main(['review', '--input', str(material), '--max-cost', 'nan']), 2)

    def test_a_dollar_capped_run_makes_one_call_at_a_time(self):
        active, peak, lock = [0], [0], threading.Lock()
        p = plan([{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'}, {'id': 'b', 'prompt': 'b.md', 'reply': 'findings'},
                  {'id': 'c', 'prompt': 'c.md', 'reply': 'findings'}, {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}])

        def fn(backend, system, message, timeout_s, model=None):
            with lock:
                active[0] += 1
                peak[0] = max(peak[0], active[0])
            import time as _t
            _t.sleep(0.02)
            with lock:
                active[0] -= 1
            if 'INSTRUCTION k' in message:
                return '\n'.join(f'{x}.{n} KEEP' for x in 'abc' for n in (1, 2)), dict(USAGE)
            return REVIEW, dict(USAGE)
        harness.run_plan(p, 'M', B, harness.Run(max_cost=10.0), call_fn=fn)
        self.assertEqual(peak[0], 1)

    @unittest.skipIf(os.name == 'nt', POSIX_MODES)
    def test_trace_folder_is_private(self):
        d = Path(tempfile.mkdtemp()) / 'trace'
        harness.Run(d)
        self.assertEqual(stat.S_IMODE(os.stat(d).st_mode), 0o700)

    def test_trace_folder_is_complete(self):
        d = Path(tempfile.mkdtemp()) / 'trace'
        fn, _ = scripted({'Review the change': REVIEW})
        run = harness.Run(d)
        harness.run_plan(pl.load_plan(None, 'review'), 'M', B, run, call_fn=fn)
        recs = [json.loads(line) for line in (d / 'trace.jsonl').read_text().splitlines()]
        self.assertEqual([r['status'] for r in recs], ['started', 'ok'])  # an interrupted call would stay 'started'
        self.assertEqual(recs[1]['usage']['cache_write'], 100)
        self.assertTrue((d / 'steps' / 'review.1.txt').exists())

    def test_design_revision_runs_only_when_the_audit_finds_issues(self):
        passes = [{'id': 'draft', 'prompt': 'd.md', 'reply': 'proposal'}, {'id': 'nums', 'prompt': 'n.md', 'reply': 'text'},
                  {'id': 'audit', 'prompt': 'a.md', 'reply': 'issues', 'inputs': ['draft', 'nums']},
                  {'id': 'revise', 'prompt': 'r.md', 'reply': 'proposal', 'inputs': ['draft', 'audit'], 'when': 'audit'}]
        p = plan(passes, 'design')
        for audit, selected in (('NONE', 'draft'), ('the draft misses the 3x growth', 'revise')):
            fn, seen = scripted({'INSTRUCTION draft': 'DRAFT', 'INSTRUCTION nums': 'NUMBERS', 'INSTRUCTION audit': audit,
                                 'INSTRUCTION revise': 'REVISED'})
            text, stats = harness.run_plan(p, 'BRIEF', B, harness.Run(), call_fn=fn)
            self.assertEqual(stats['selected'], selected)
            audit_message = next(m for _, m in seen if 'INSTRUCTION audit' in m)
            self.assertIn('DRAFT', audit_message)
            self.assertIn('NUMBERS', audit_message)



class AssemblyTest(unittest.TestCase):
    def setUp(self):
        self.p = plan([{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'}, {'id': 'b', 'prompt': 'b.md', 'reply': 'findings'},
                       {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts', 'batch': 2}])
        self.replies = {'a': REVIEW, 'b': REVIEW.replace('Bug one', 'Bug three').replace('Bug two', 'Bug four')}

    def test_verdicts_keep_drop_open_cut_and_duplicates(self):
        self.replies['k'] = ('a.1 KEEP - q\na.1 CUT "Side claim here."\na.2 REFUTED - q\nb.1 DUP a.1 - same\n'
                             'b.2 OPEN - needs unseen code')
        text, stats = pl.assemble(self.p, self.replies)
        self.assertIn('### 1. Bug one', text)
        self.assertNotIn('Side claim', text)
        self.assertNotIn('Bug two', text)
        self.assertIn('- Bug four', text.split('Open questions')[1])
        self.assertEqual((stats['kept'], stats['open'], stats['cuts_not_found']), (1, 1, 0))

    def test_findings_keep_their_own_text(self):
        self.replies['k'] = 'a.1 KEEP\na.2 KEEP\nb.1 KEEP\nb.2 KEEP'
        text, _ = pl.assemble(self.p, self.replies)
        for title in ('Bug one', 'Bug two', 'Bug three', 'Bug four'):
            self.assertIn(title, text)
        self.assertIn('runtime', text.split('Not checked')[1])

    def test_checked_findings_preserve_code_whitespace(self):
        self.replies['a'] = REVIEW.replace('`a`', '`a  b`\n```python\n    return "a  b"\n```')
        self.replies['k'] = 'a.1 KEEP\na.2 KEEP\nb.1 KEEP\nb.2 KEEP'
        text, _ = pl.assemble(self.p, self.replies)
        self.assertIn('`a  b`\n```python\n    return "a  b"\n```', text)

    def test_an_earlier_no_defects_line_does_not_contradict_kept_findings(self):
        self.replies['a'] = 'No defects found.'
        self.replies['k'] = 'b.1 KEEP\nb.2 KEEP'
        text, stats = pl.assemble(self.p, self.replies)
        self.assertEqual(stats['kept'], 2)
        self.assertNotIn('No defects found', text)

    def test_numbered_open_questions_are_not_findings(self):
        text = '### 1. Defect\np\n### Open questions\n### 1. Is the service reachable?\nUnknown.\n### Not checked\nruntime'
        _, found, sections = pl.findings('a', text)
        self.assertEqual(list(found), ['a.1'])
        self.assertIn('Is the service reachable?', sections['open questions'])

    def test_duplicate_targets_must_be_real_findings_kept_directly(self):
        for verdict in ('a.1 KEEP\na.2 DUP a.1\nb.1 DUP a.2\nb.2 KEEP',
                        'a.1 KEEP\na.2 KEEP\nb.1 DUP ghost.1\nb.2 KEEP\nghost.1 KEEP'):
            self.replies['k'] = verdict
            with self.subTest(verdict=verdict), self.assertRaises(pl.NeedVerdicts) as cm:
                pl.assemble(self.p, self.replies)
            self.assertEqual(cm.exception.ids, ['b.1'])

    def test_no_output_pass_running_is_a_clear_failure(self):
        with self.assertRaisesRegex(ValueError, 'no findings pass ran'):
            pl.assemble(self.p, {})
        p = {'kind': 'design', 'passes': [{'id': 'draft', 'reply': 'proposal'}]}
        with self.assertRaisesRegex(ValueError, 'no proposal pass ran'):
            pl.assemble(p, {'audit': 'NONE'})

    def test_task_and_plan_kind_must_match(self):
        with self.assertRaisesRegex(ValueError, 'review task needs a review plan'):
            pl.load_plan(pl.PLANS / 'design.json', 'review')

    def test_malformed_dependencies_are_validation_errors(self):
        for fields in ({'inputs': [[]]}, {'when': []}):
            with self.subTest(fields=fields), self.assertRaises(ValueError):
                plan([{'id': 'a', 'prompt': 'a.md', 'reply': 'findings', **fields}])

    def test_missing_verdicts_and_orphan_duplicates_need_the_checker_again(self):
        self.replies['k'] = 'a.1 KEEP\na.2 KEEP\nb.1 KEEP'
        with self.assertRaises(pl.NeedVerdicts) as cm:
            pl.assemble(self.p, self.replies)
        self.assertEqual(cm.exception.ids, ['b.2'])
        self.replies['k'] = 'a.1 REFUTED\na.2 KEEP\nb.1 DUP a.1\nb.2 KEEP'
        with self.assertRaises(pl.NeedVerdicts) as cm:
            pl.assemble(self.p, self.replies)
        self.assertEqual((cm.exception.ids, cm.exception.reason), (['b.1'], 'duplicate of a finding that was not kept'))

    def test_repair_round_fixes_an_orphan_in_the_harness(self):
        fn, _ = scripted({'INSTRUCTION a': REVIEW, 'INSTRUCTION b': self.replies['b'],
                          'still need a verdict': 'b.1 KEEP - judged on its own',
                          'INSTRUCTION k': lambda m: 'a.1 REFUTED\na.2 KEEP' if 'a.1' in m.split('this batch:')[1].split('.\n')[0]
                          else 'b.1 DUP a.1\nb.2 KEEP'})
        text, stats = harness.run_plan(self.p, 'M', B, harness.Run(), call_fn=fn)
        self.assertIn('Bug three', text)
        self.assertEqual(stats['kept'], 3)

    def test_a_batched_plan_bounds_findings_per_reply_so_checker_calls_come_from_the_plan(self):
        many = ''.join(f'### {i}. Bug {i} (`a.py:{i}`)\n- Problem: p.\n\n' for i in range(1, pl.MAX_FINDINGS + 2))
        with self.assertRaisesRegex(ValueError, f'more than the {pl.MAX_FINDINGS}'):
            pl.check_reply(self.p, self.p['passes'][0], many)
        unbatched = plan([{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'},
                          {'id': 'b', 'prompt': 'b.md', 'reply': 'findings'},
                          {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}])
        pl.check_reply(unbatched, unbatched['passes'][0], many)  # one checker call however many findings
        fn, seen = scripted({'INSTRUCTION a': many, 'INSTRUCTION b': REVIEW, 'INSTRUCTION k': 'a.1 KEEP'})
        run = harness.Run()
        with self.assertRaisesRegex(ValueError, 'more than the'):
            harness.run_plan(self.p, 'M', B, run, call_fn=fn)
        self.assertLessEqual(run.calls, 4)  # a and b, a retried once; never one checker call per runaway heading

    def test_batches_only_count_verdicts_for_their_own_findings(self):
        texts = ['a.1 KEEP\na.2 KEEP\nb.1 REFUTED', 'b.1 KEEP\nb.2 KEEP']  # batch 1 oversteps on b.1
        merged = pl.combine(self.p, self.p['passes'][2], self.replies, texts)
        self.assertEqual(pl.verdicts(merged)[0]['b.1'][0], 'KEEP')
        self.assertEqual(len(pl.requests(self.p, self.p['passes'][2], self.replies)), 2)

    def test_plans_are_validated(self):
        d = tempfile.mkdtemp()
        bad = [
            [{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'}, {'id': 'a', 'prompt': 'a.md', 'reply': 'findings'}],
            [{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'}, {'id': 'b', 'prompt': 'b.md', 'reply': 'findings'}],
            [{'id': 'a', 'prompt': 'a.md', 'reply': 'findings', 'inputs': ['z']}],
            [{'id': 'a', 'prompt': '../x.md', 'reply': 'findings'}],
            [{'id': 'a', 'prompt': 'a.md', 'reply': 'findings', 'extra': 1}],
            [{'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}, {'id': 'a', 'prompt': 'a.md', 'reply': 'findings'},
             {'id': 'b', 'prompt': 'b.md', 'reply': 'findings'}],
        ]
        for passes in bad:
            with self.subTest(passes=passes), self.assertRaises(ValueError):
                for q in passes:
                    if '/' not in q['prompt']:
                        Path(d, q['prompt']).write_text('x', encoding='utf-8')
                pl.validate({'version': 1, 'kind': 'review', 'role': 'r', 'passes': passes}, d)
        self.assertEqual(pl.load_plan(None, 'design')['passes'][0]['reply'], 'proposal')


class CheckerBackendTest(unittest.TestCase):
    """--checker-backend: the verdicts pass and its repair run on a second backend under the same Run."""
    FINDER = {'name': 'finder', 'argv': ['finder'], 'style': 'plain'}
    CHECKER = {'name': 'checker', 'argv': ['checker'], 'style': 'plain'}

    def passes(self, **checker):
        return [{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'}, {'id': 'b', 'prompt': 'b.md', 'reply': 'findings'},
                {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts', **checker}]

    def test_verdicts_and_repair_go_to_the_checker_and_the_trace_names_each_backend(self):
        d = Path(tempfile.mkdtemp()) / 'trace'
        verdicts = ['a.1 KEEP\na.2 KEEP\nb.1 KEEP', 'b.2 KEEP']  # the first reply misses b.2: one repair
        seen, lock = [], threading.Lock()
        def fn(b, system, message, timeout_s, model=None):
            with lock:
                seen.append(b['name'])
                return (verdicts.pop(0) if b['name'] == 'checker' else REVIEW), dict(USAGE)
        run = harness.Run(d)
        text, stats = harness.run_plan(plan(self.passes()), 'M', self.FINDER, run, call_fn=fn, checker_spec=self.CHECKER)
        self.assertEqual(stats['kept'], 4)
        self.assertEqual(sorted(seen), ['checker', 'checker', 'finder', 'finder'])
        self.assertEqual(run.summary()['calls'], 4)
        recs = [json.loads(line) for line in (d / 'trace.jsonl').read_text().splitlines()]
        self.assertTrue(all('backend' in r for r in recs if r['status'] != 'skipped'))
        self.assertEqual({r['step']: r['backend'] for r in recs if r['status'] == 'ok'},
                         {'a': 'finder', 'b': 'finder', 'k': 'checker', 'k.repair': 'checker'})

    def test_a_checker_without_a_verdicts_pass_is_refused_before_any_call(self):
        calls = []
        for p in (pl.load_plan(None, 'review'), pl.load_plan(None, 'design')):
            with self.subTest(kind=p['kind']), self.assertRaisesRegex(ValueError, 'verdicts pass'):
                harness.run_plan(p, 'M', B, harness.Run(), call_fn=lambda *a: calls.append(a), checker_spec=self.CHECKER)
        self.assertEqual(calls, [])
        with tempfile.TemporaryDirectory() as t, mock.patch.object(harness, 'call') as call, \
                mock.patch('sys.stderr', new_callable=io.StringIO) as err:
            material = Path(t, 'm'); material.write_text('M')
            self.assertEqual(harness.main(['review', '--input', str(material), '--checker-backend', 'checker']), 2)
            call.assert_not_called()
        self.assertIn('verdicts pass', err.getvalue())

    def test_a_per_pass_model_follows_the_backend_that_runs_the_pass(self):
        p = plan(self.passes(model='small'))
        models = []
        def fn(b, system, message, timeout_s, model=None):
            models.append((b['name'], model))
            return ('a.1 KEEP\na.2 KEEP\nb.1 KEEP\nb.2 KEEP' if b['name'] == 'checker' else REVIEW), dict(USAGE)
        harness.run_plan(p, 'M', self.FINDER, harness.Run(), call_fn=fn, checker_spec={**self.CHECKER, 'style': 'claude'})
        self.assertEqual(sorted(models), [('checker', 'small'), ('finder', None), ('finder', None)])
        with self.assertRaisesRegex(ValueError, 'per-pass model'):
            harness.run_plan(p, 'M', {**self.FINDER, 'style': 'claude'}, harness.Run(), call_fn=fn, checker_spec=self.CHECKER)

    def test_each_backend_and_model_warms_its_own_cache_once(self):
        p = plan([{'id': 'a', 'prompt': 'a.md', 'reply': 'findings', 'model': 'm'},
                  {'id': 'b', 'prompt': 'b.md', 'reply': 'findings', 'model': 'm'},
                  {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts', 'batch': 1, 'model': 'm'}])
        cold, warmed, lock = [], set(), threading.Lock()
        def fn(b, s, m, timeout_s, model=None):
            key = (b['name'], model)
            with lock:
                if key not in warmed:
                    cold.append(key)
            time.sleep(.01)
            with lock:
                warmed.add(key)
            return ('\n'.join(f'{x}.{n} KEEP' for x in 'ab' for n in [1, 2]) if b['name'] == 'checker' else REVIEW), USAGE
        harness.run_plan(p, 'M', {**self.FINDER, 'style': 'claude'}, harness.Run(), call_fn=fn,
                         checker_spec={**self.CHECKER, 'style': 'claude'})
        self.assertEqual(sorted(cold), [('checker', 'm'), ('finder', 'm')])

    def test_both_backends_share_one_cost_cap(self):
        p = plan([{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'}, {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}])
        def fn(b, system, message, timeout_s, model=None):  # the finder reports no price, the checker 0.02
            return ('a.1 KEEP\na.2 KEEP', {**USAGE, 'cost_usd': 0.02}) if b['name'] == 'checker' else (REVIEW, None)
        run = harness.Run(max_cost=1, unpriced_call_usd=0.05)
        harness.run_plan(p, 'M', self.FINDER, run, call_fn=fn, checker_spec=self.CHECKER)
        self.assertEqual((run.summary()['calls'], run.summary()['cost_usd']), (2, 0.07))
        run = harness.Run(max_cost=0.05, unpriced_call_usd=0.05)
        with self.assertRaisesRegex(harness.BudgetExceeded, 'cost cap'):
            harness.run_plan(p, 'M', self.FINDER, run, call_fn=fn, checker_spec=self.CHECKER)
        self.assertEqual(run.summary()['calls'], 1)

    def test_cli_checker_gets_codex_isolation_and_the_trace_records_both_backends(self):
        t = Path(tempfile.mkdtemp())
        for name in ('a.md', 'k.md'):
            (t / name).write_text(f'INSTRUCTION {name}')
        (t / 'plan.json').write_text(json.dumps({'version': 1, 'kind': 'review', 'role': 'r', 'prompts_dir': '.',
                                                 'passes': [{'id': 'a', 'prompt': 'a.md', 'reply': 'findings'},
                                                            {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}]}))
        (t / 'm').write_text('M')
        events = '\n'.join(json.dumps(x) for x in [
            {'type': 'item.completed', 'item': {'type': 'agent_message', 'text': 'a.1 KEEP\na.2 KEEP'}},
            {'type': 'turn.completed', 'usage': {'input_tokens': 10, 'output_tokens': 2}}])
        calls = []
        def isolated(argv, stdin, timeout, **kw):
            assert isinstance(kw.pop('watch'), harness.Watch)  # every pass gets a soft limit
            calls.append((argv[0], kw))
            return events if kw.get('codex_home') else REVIEW
        trace = Path(tempfile.mkdtemp()) / 'trace'
        with mock.patch.object(backends, 'run_isolated', side_effect=isolated), \
                mock.patch('sys.stderr', new_callable=io.StringIO) as err, \
                mock.patch('sys.stdout', new_callable=io.StringIO):
            code = harness.main(['review', '--input', str(t / 'm'), '--pipeline', str(t / 'plan.json'),
                                 '--backend', 'finder-cmd', '--checker-backend', 'codex-chatgpt', '--trace', str(trace)])
        self.assertEqual(code, 0, err.getvalue())
        self.assertEqual(calls[0], ('finder-cmd', {}))
        self.assertEqual(calls[1][1], {'codex_home': True, 'subscription_auth': True, 'lean': True})
        summary = json.loads(err.getvalue().strip().splitlines()[-1].split('harness: ', 1)[1])
        self.assertEqual((summary['backend'], summary['checker_backend'], summary['calls']), ('finder-cmd', 'codex-chatgpt', 2))
        self.assertEqual(json.loads((trace / 'backends.json').read_text()),
                         {'backend': 'finder-cmd', 'checker_backend': 'codex-chatgpt'})
        self.assertEqual(json.loads((trace / 'summary.json').read_text())['checker_backend'], 'codex-chatgpt')


if __name__ == '__main__':
    unittest.main()
