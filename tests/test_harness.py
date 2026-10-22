"""Offline checks of harness.py and pipelines.py: fake backends only, no model calls."""
import json
import os
import shlex
import stat
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import backends  # noqa: E402
import harness  # noqa: E402
import pipelines as pl  # noqa: E402

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
        d = tempfile.mkdtemp()
        exe = Path(d, 'claude')
        exe.write_text(f'#!{sys.executable}\n{body}', encoding='utf-8')
        exe.chmod(exe.stat().st_mode | stat.S_IEXEC)
        return str(exe)

    def test_claude_gets_material_in_a_system_prompt_file_and_reports_usage(self):
        exe = self.fake_claude(
            'import sys, json\na = sys.argv\nf = a[a.index("--system-prompt-file") + 1]\n'
            'assert a[a.index("--output-format") + 1] == "json"\n'
            'print(json.dumps({"result": open(f).read() + "|" + sys.stdin.read(), "total_cost_usd": 0.02,\n'
            '  "usage": {"input_tokens": 3, "cache_creation_input_tokens": 7, "cache_read_input_tokens": 90, "output_tokens": 4}}))\n')
        text, usage = harness.call({'name': 'c', 'argv': [exe, '-p'], 'style': 'claude'}, 'SYSTEM+MATERIAL', 'INSTRUCTION', 30)
        self.assertEqual(text, 'SYSTEM+MATERIAL|INSTRUCTION')
        self.assertEqual(usage, {'input': 3, 'cache_write': 7, 'cache_read': 90, 'output': 4, 'cost_usd': 0.02, 'models': []})

    def test_claude_errors_and_non_json_raise(self):
        bad = self.fake_claude('import json\nprint(json.dumps({"is_error": True, "result": "limit reached"}))\n')
        with self.assertRaisesRegex(backends.JudgeError, 'limit reached'):
            harness.call({'name': 'c', 'argv': [bad], 'style': 'claude'}, 's', 'm', 30)
        junk = self.fake_claude('print("not json")\n')
        with self.assertRaisesRegex(backends.JudgeError, 'did not return JSON'):
            harness.call({'name': 'c', 'argv': [junk], 'style': 'claude'}, 's', 'm', 30)

    def test_other_backends_get_system_then_message_on_stdin(self):
        argv = shlex.split(shlex.join([sys.executable, '-c', 'import sys; print(sys.stdin.read())']))
        text, usage = harness.call({'name': 'py', 'argv': argv, 'style': 'plain'}, 'SYS', 'MSG', 30)
        self.assertEqual((text.strip(), usage), ('SYS\n\nMSG', None))
        with self.assertRaisesRegex(ValueError, 'claude-style'):
            harness.call({'name': 'py', 'argv': argv, 'style': 'plain'}, 'SYS', 'MSG', 30, model='haiku')

    def test_backend_styles_are_declared_not_guessed(self):
        self.assertEqual(harness.backend('claude'), {'name': 'claude', 'argv': shlex.split(backends.JUDGE_PRESETS['claude']),
                                                     'style': 'claude'})
        self.assertEqual(harness.backend('codex')['style'], 'plain')
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
        invoke.assert_called_once_with(backend['argv'], 'SYS\n\nMSG', 30, subscription_auth=True)
        self.assertEqual(text, 'No defects found.')
        self.assertEqual(usage, {'input': 30, 'cache_read': 70, 'cache_write': 0, 'output': 12, 'cost_usd': None})

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

    def test_call_cap_must_be_a_positive_integer(self):
        for bad in (True, 0, -1, 1.5, float('nan')):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                harness.Run(max_calls=bad)

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
        run = harness.Run(max_calls=3)
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

    def test_a_command_that_cannot_start_is_not_retried(self):
        calls = []

        def fn(backend, system, message, timeout_s, model=None):
            calls.append(1)
            raise FileNotFoundError('no such binary')
        with self.assertRaises(FileNotFoundError):
            harness.run_plan(pl.load_plan(None, 'review'), 'M', B, harness.Run(), call_fn=fn)
        self.assertEqual(len(calls), 1)

    def test_budget_caps_stop_new_calls(self):
        run = harness.Run(max_calls=1)
        run.reserve()
        with self.assertRaisesRegex(harness.BudgetExceeded, 'call cap'):
            run.reserve()
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
        exe = Path(tempfile.mkdtemp(), 'claude')
        exe.write_text(f'#!{sys.executable}\nimport json, sys\nprint(json.dumps({{"is_error": True, "result": "boom", '
                       f'"total_cost_usd": 0.02, "usage": {{"input_tokens": 50}}}}))\nsys.exit(1)\n')
        exe.chmod(0o755)
        with self.assertRaises(harness.CallError) as cm:
            harness.call({'name': 'c', 'argv': [str(exe)], 'style': 'claude'}, 's', 'm', 30)
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

    def test_trace_folder_is_private_and_complete(self):
        d = Path(tempfile.mkdtemp()) / 'trace'
        fn, _ = scripted({'Review the change': REVIEW})
        run = harness.Run(d)
        harness.run_plan(pl.load_plan(None, 'review'), 'M', B, run, call_fn=fn)
        self.assertEqual(stat.S_IMODE(os.stat(d).st_mode), 0o700)
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


if __name__ == '__main__':
    unittest.main()
