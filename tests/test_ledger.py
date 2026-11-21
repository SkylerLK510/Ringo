"""Offline checks of ledger.py with fixture rows and events: no model calls."""
import contextlib
import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import backends  # noqa: E402
import ledger  # noqa: E402

NOW = 1_000_000
SOL = {'input': 1_000_000, 'cache_read': 2_000_000, 'cache_write': 0, 'output': 100_000}


def run(*argv):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            code = ledger.main(list(argv))
        except SystemExit as e:  # argparse
            code = e.code
    return code, out.getvalue(), err.getvalue()


def lines(*rows):
    return '\n'.join(r if isinstance(r, str) else json.dumps(r, separators=(',', ':')) for r in rows) + '\n'


def group(agent, surface, model, jobs, calls, unknown_calls, counts, unit, value, unknown_models=(), ctx=None, unknown_jobs=0):
    parts = {k: (ctx or {}).get(k, 0) for k in ('fixed', 'carry', 'growth')}
    total = sum(parts.values())
    return {'agent': agent, 'surface': surface, 'model': model, 'jobs': jobs, 'calls': calls, 'calls_unknown_jobs': unknown_calls,
            'unknown_jobs': unknown_jobs, **counts, 'T': sum(counts.values()),
            'W': {'unit': unit, 'value': value, 'unknown_models': list(unknown_models)},
            'ctx': {'jobs_with_ctx': 1 if ctx else 0, **parts,
                    'shares': {k: round(v / total, 4) if total else None for k, v in parts.items()},
                    'bases': [ctx['base']] if ctx else []}}


def counts(i=0, r=0, w=0, o=0):
    return {'input': i, 'cache_read': r, 'cache_write': w, 'output': o}


class ReportTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.d = Path(self.dir.name)

    def usage(self, *rows):
        path = self.d / 'usage.jsonl'
        path.write_text(lines(*rows), encoding='utf-8')
        return str(path)

    def trace(self, name='job'):
        job = self.d / name
        (job / 'trace').mkdir(parents=True)
        (job / 'trace' / 'trace.jsonl').write_text(lines(
            {'step': 'a', 'attempt': 1, 'backend': 'claude', 'status': 'ok', 'usage': {'input': 1000, 'cache_read': 10000, 'cache_write': 2000, 'output': 500, 'surface': 'cli:claude'}},
            {'step': 'b', 'attempt': 1, 'backend': 'codex-chatgpt@gpt-6-luna', 'model': 'gpt-6-luna', 'status': 'ok', 'usage': SOL},
            {'step': 'c', 'attempt': 1, 'backend': 'claude', 'status': 'started'}), encoding='utf-8')
        return str(job)

    def test_old_rows_read_like_new_rows(self):
        old = {'at': NOW - 5, 'agent': 'codex', 'job': '/j/1', 'model': 'gpt-6.1-sol', **SOL}
        new = {**old, 'job': '/j/2', 'surface': 'cli:codex', 'billing': 'subscription', 'calls': 4}
        a, b = ledger.from_row(old), ledger.from_row(new)
        self.assertEqual((a['counts'], a['surface']), (b['counts'], b['surface']))
        self.assertTrue(a['surface_inferred'] and not b['surface_inferred'])
        self.assertIsNone(a['calls'], 'an old row never said how many calls: unknown, not zero')
        self.assertEqual(b['calls'], 4)

    def test_report_json_exact(self):
        job = self.trace()
        rows = [
            {'at': NOW - 100, 'agent': 'codex', 'job': '/j/1', 'model': 'gpt-6.1-sol', **SOL, 'surface': 'cli:codex', 'billing': 'subscription',
             'calls': 3, 'ctx_base': 10000, 'ctx_fixed': 29000, 'ctx_carry': 4000, 'ctx_growth': 3000},
            {'at': NOW - 200, 'agent': 'codex', 'job': '/j/2', 'model': 'gpt-7', 'input': 10, 'cache_read': 20, 'cache_write': 0, 'output': 5},
            {'at': NOW - 3 * 86400, 'agent': 'claude', 'job': '/j/3', 'model': 'claude-x', 'input': 1000, 'cache_read': 10000,
             'cache_write': 2000, 'output': 500, 'surface': 'cli:claude', 'calls': 2},
            {'at': NOW - 50, 'agent': 'agy', 'job': '/j/4', 'input': 7, 'cache_read': 0, 'cache_write': 0, 'output': 3},
            {'at': NOW - 10, 'agent': 'claude', 'job': '/j/5', 'unknown': True, 'surface': 'cli:claude'},
            {'at': NOW - 5, 'agent': 'ringo', 'job': job, 'input': 1, 'cache_read': 1, 'cache_write': 1, 'output': 1},
            '{"at": 1,',
        ]
        code, out, err = run('report', '--usage', self.usage(*rows), '--now', str(NOW), '--json')
        self.assertEqual((code, err), (0, ''))
        doc = json.loads(out)
        agy = group('agy', 'cli:agy', None, 1, 0, 1, counts(7, 0, 0, 3), None, None)
        claude_unknown = group('claude', 'cli:claude', None, 1, 0, 1, counts(), 'claude_weighted_tokens', None, unknown_jobs=1)
        claude = group('claude', 'cli:claude', 'claude-x', 1, 2, 0, counts(1000, 10000, 2000, 500), 'claude_weighted_tokens', 8000.0)
        sol = group('codex', 'cli:codex', 'gpt-6.1-sol', 1, 3, 0, counts(1_000_000, 2_000_000, 0, 100_000), 'codex_credits', 80.0,
                    ctx={'base': 10000, 'fixed': 29000, 'carry': 4000, 'growth': 3000})
        gpt7 = group('codex', 'cli:codex', 'gpt-7', 1, 0, 1, counts(10, 20, 0, 5), 'codex_credits', None, ['gpt-7'])
        p_claude = group('ringo', 'cli:claude', None, 1, 1, 0, counts(1000, 10000, 2000, 500), 'claude_weighted_tokens', 8000.0)
        p_luna = group('ringo', 'cli:codex', 'gpt-6-luna', 1, 1, 0, counts(1_000_000, 2_000_000, 0, 100_000), 'codex_credits', 4.25)
        self.assertEqual(doc['source'], {'path': str(self.d / 'usage.jsonl'), 'rows': 6, 'skipped_lines': 1, 'traces_expanded': 1})
        self.assertEqual([w['window'] for w in doc['windows']], ['24h', '7d', 'all'])
        self.assertEqual([w['since'] for w in doc['windows']], [NOW - 86400, NOW - 7 * 86400, None])
        self.assertEqual(doc['windows'][0]['groups'], [agy, claude_unknown, sol, gpt7, p_claude, p_luna])
        self.assertEqual(doc['windows'][1]['groups'], [agy, claude_unknown, claude, sol, gpt7, p_claude, p_luna])
        self.assertEqual(doc['windows'][2]['groups'], doc['windows'][1]['groups'])
        self.assertEqual(list(doc), ['source', 'windows'])

    def test_pipeline_rows_expand_from_their_trace(self):
        job = self.trace()
        usage = self.usage({'at': NOW - 5, 'agent': 'ringo', 'job': job, 'input': 9, 'cache_read': 9, 'cache_write': 9, 'output': 9})
        doc = json.loads(run('report', '--usage', usage, '--now', str(NOW), '--window', 'all', '--json')[1])
        groups = doc['windows'][0]['groups']
        self.assertEqual([(g['surface'], g['model'], g['calls']) for g in groups], [('cli:claude', None, 1), ('cli:codex', 'gpt-6-luna', 1)],
                         'one entry per paid attempt: the started record is not one')
        self.assertEqual(sum(g['T'] for g in groups), 13_500 + 3_100_000, 'the row'"'"'s own counts are replaced, not added')

    def test_no_traces_keeps_the_row(self):
        usage = self.usage({'at': NOW - 5, 'agent': 'ringo', 'job': self.trace(), 'input': 9, 'cache_read': 9, 'cache_write': 9, 'output': 9})
        doc = json.loads(run('report', '--usage', usage, '--now', str(NOW), '--window', 'all', '--no-traces', '--json')[1])
        (g,) = doc['windows'][0]['groups']
        self.assertEqual((g['surface'], g['T'], g['W']['unit'], doc['source']['traces_expanded']), ('pipeline', 36, None, 0))

    def test_extra_trace_folders_add_their_attempts(self):
        self.trace('one')
        usage = self.usage()
        doc = json.loads(run('report', '--usage', usage, '--now', str(NOW), '--window', 'all', '--trace', str(self.d / 'one' / 'trace'), '--json')[1])
        self.assertEqual(sum(g['calls'] for g in doc['windows'][0]['groups']), 2)

    def test_w_unknown_for_unknown_model_and_missing_counts(self):
        rows = [{'at': NOW - 1, 'agent': 'codex', 'job': '/a', **SOL},  # no model named
                {'at': NOW - 1, 'agent': 'codex', 'job': '/b', 'model': 'gpt-6.1-sol', 'input': 5},  # no complete counts
                {'at': NOW - 1, 'agent': 'codex', 'job': '/c', 'model': 'gpt-6.1-sol', **SOL}]
        doc = json.loads(run('report', '--usage', self.usage(*rows), '--now', str(NOW), '--window', 'all', '--json')[1])
        by = {g['model']: g for g in doc['windows'][0]['groups']}
        self.assertEqual(by[None]['W'], {'unit': 'codex_credits', 'value': None, 'unknown_models': ['(not named)']})
        self.assertEqual(by['gpt-6.1-sol']['W'], {'unit': 'codex_credits', 'value': None, 'unknown_models': []},
                         'one job without complete counts makes the whole group unknown, never a partial sum')

    def test_bad_arguments_and_missing_files_exit_2(self):
        self.assertEqual(run('report', '--usage', str(self.d / 'none.jsonl'))[0], 2)
        self.assertEqual(run('report', '--usage', self.usage(), '--window', '5x')[0], 2)
        self.assertEqual(run('report', '--usage', self.usage(), '--window', '0h')[0], 2)
        for base in ('0', '-5', 'x'):
            self.assertEqual(run('attribute', '--shell', str(self.d), '--base', base)[0], 2, base)

    def test_cli_text_output_has_header_and_tables(self):
        code, out, _ = run('report', '--usage', self.usage({'at': NOW - 1, 'agent': 'codex', 'job': '/a', 'model': 'gpt-6.1-sol', **SOL}),
                           '--now', str(NOW))
        self.assertEqual(code, 0)
        first = out.splitlines()[0]
        self.assertTrue(first.startswith('ledger: ') and '1 rows' in first and first.endswith('0 model calls'), first)
        self.assertEqual([l for l in out.splitlines() if l.startswith('window ')], ['window 24h', 'window 7d', 'window all'])
        self.assertEqual(len([l for l in out.splitlines() if l.startswith('agent ')]), 3)
        self.assertIn('80.0 codex_credits', out)

    def test_makes_no_model_call(self):
        shell = self.d / 'shell' / '1' / 'jobs' / '1'
        shell.mkdir(parents=True)
        (shell / 'events.jsonl').write_text(lines({'id': 1, 'result': {}}), encoding='utf-8')
        boom = mock.Mock(side_effect=AssertionError('a model call'))
        with mock.patch.object(backends, 'run_isolated', boom), mock.patch.object(subprocess, 'run', boom), \
                mock.patch.object(subprocess, 'Popen', boom):
            self.assertEqual(run('report', '--usage', self.usage(), '--json')[0], 0)
            self.assertEqual(run('attribute', '--shell', str(self.d / 'shell'), '--base', '1000')[0], 0)
        boom.assert_not_called()


def codex_events(*extra):
    def upd(total_in, last_in, turn=None):
        params = {'tokenUsage': {'total': {'inputTokens': total_in, 'outputTokens': 1}, 'last': {'inputTokens': last_in, 'outputTokens': 1}}}
        if turn:
            params['turnId'] = turn
        return {'method': 'thread/tokenUsage/updated', 'params': params}
    return lines({'id': 1, 'result': {}}, {'id': 2, 'result': {'thread': {'id': 'T'}}}, *extra), upd


class AttributeTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.d = Path(self.dir.name)

    def events(self, n, text):
        job = self.d / 'shell' / '1' / 'jobs' / str(n)
        job.mkdir(parents=True, exist_ok=True)
        (job / 'events.jsonl').write_text(text, encoding='utf-8')

    def attribute(self, *argv):
        code, out, err = run('attribute', '--state', str(self.d), '--json', *argv)
        return code, (json.loads(out) if out else None), err

    def test_attribute_codex_rules(self):
        _, upd = codex_events()
        text = lines({'id': 1, 'result': {}},
                     {'id': 2, 'result': {'thread': {'id': 'T'}}},
                     upd(40, 40),                            # before the turn is answered: a resumed thread re-announcing
                     {'id': 3, 'result': {'turn': {'id': 'U'}}},
                     upd(50, 50, 'OLD'),                      # a replay of another turn
                     upd(20, 20),                             # the total went down
                     upd(100, 12_000),                        # a call
                     upd(100, 12_000),                        # the same total again: the same call, which no one counts twice
                     upd(20, 20),                             # down again
                     upd(100, 12_000),                        # an accepted total returning after a bad report: a repeat
                     upd(250, 15_000), upd(400, 9_000))
        self.events(1, text)
        code, doc, _ = self.attribute('--agent', 'codex', '--base', '10000')
        self.assertEqual(code, 0)
        a = doc['agents']['codex']
        self.assertEqual((a['calls'], a['jobs'], a['total']), (3, 1, 36_000))
        self.assertEqual((a['fixed'], a['carry'], a['growth']), (29_000, 4_000, 3_000))
        self.assertEqual(a['skipped'], {'before_turn': 1, 'other_turn': 1, 'behind_or_malformed': 2, 'repeat': 1})
        self.assertEqual(a['calls'] + sum(a['skipped'].values()), a['decompose_calls'], 'every report decompose.py counts is a call or a named skip')
        self.assertEqual(a['surplus'], 0)

    def test_attribute_surplus_matches_decompose_when_first_call_is_below_base(self):
        _, upd = codex_events()
        self.events(1, lines({'id': 1, 'result': {}}, {'id': 2, 'result': {'thread': {'id': 'T'}}}, {'id': 3, 'result': {'turn': {'id': 'U'}}},
                             upd(10, 8_000), upd(20, 20_000)))
        a = self.attribute('--agent', 'codex', '--base', '10000')[1]['agents']['codex']
        self.assertEqual((a['fixed'], a['carry'], a['growth'], a['total'], a['surplus']), (18_000, 0, 12_000, 28_000, 2_000))

    def test_attribute_claude_rules(self):
        def msg(i, parent=None, **use):
            return {'type': 'assistant', 'parent_tool_use_id': parent, 'message': {'id': i, 'usage': use, 'content': []}}
        self.events(2, lines({'type': 'system', 'subtype': 'init'},
                             msg('m1', input_tokens=5, cache_read_input_tokens=100, cache_creation_input_tokens=20, output_tokens=1),
                             msg('m1', input_tokens=5, cache_read_input_tokens=100, cache_creation_input_tokens=20, output_tokens=9),
                             msg('s1', 'toolu_1', input_tokens=999),
                             msg('m2', input_tokens=1, cache_read_input_tokens=200, cache_creation_input_tokens=0)))
        code, doc, _ = self.attribute('--agent', 'claude', '--base', '100')
        a = doc['agents']['claude']
        self.assertEqual((code, a['calls'], a['total']), (0, 2, 125 + 201))
        self.assertEqual((a['fixed'], a['carry'], a['growth']), (200, 50, 76))

    def test_attribute_needs_a_base(self):
        _, upd = codex_events()
        self.events(1, lines({'id': 1, 'result': {}}))
        code, doc, err = self.attribute('--agent', 'codex')
        self.assertEqual((code, doc), (2, None))
        self.assertIn('--base', err)
        # a measured base in usage.jsonl is used
        (self.d / 'usage.jsonl').write_text(lines({'agent': 'codex', 'ctx_base': 7}, {'agent': 'codex', 'ctx_base': 9}, {'agent': 'claude', 'ctx_base': 1}))
        self.assertEqual(self.attribute('--agent', 'codex')[1]['agents']['codex']['base'], 9)

    def test_attribute_text_output(self):
        _, upd = codex_events()
        self.events(1, lines({'id': 1, 'result': {}}, {'id': 2, 'result': {'thread': {'id': 'T'}}}, {'id': 3, 'result': {'turn': {'id': 'U'}}}, upd(100, 12_000)))
        code, out, _ = run('attribute', '--state', str(self.d), '--agent', 'codex', '--base', '10000')
        self.assertEqual(code, 0)
        self.assertIn('codex: base 10,000;', out)
        self.assertIn('identity: 1 + 0 = 1', out)


if __name__ == '__main__':
    unittest.main()
