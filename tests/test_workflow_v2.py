"""v2 work loop: text protocol, role options, spec tests with disputes, and the document review plan."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import harness
import workflow

SPEC_TESTS = '''```python
import unittest
from value import f


class Brief(unittest.TestCase):
    def test_doubles_integers(self):
        """"Return twice x." """
        self.assertEqual(f(5), 10)

    def test_keeps_floats(self):
        """"Return twice x." """
        self.assertEqual(f(0.5), 1.0)
        self.assertIsInstance(f(0.5), float)
```'''
REVIEW_OK = 'What this change does: adds f.\n\nNo defects found.'


class TextProtocolTest(unittest.TestCase):
    def test_blocks_become_files_edits_and_disputes(self):
        reply = workflow.parse_text_reply(
            'Sure.\n@@@ RATIONALE\nDouble it.\n@@@ FILE a.py\ndef f(x):\n    return 2*x\n\n'
            '@@@ REPLACE b.py\nx = 1\n\n@@@ WITH\nx = 2\n@@@ END\n@@@ DISPUTE Brief.test_odd\n"Return twice x."\n')
        self.assertEqual(reply['rationale'], 'Double it.')
        self.assertEqual(reply['files'], {'a.py': 'def f(x):\n    return 2*x\n'})
        self.assertEqual(reply['edits'], [{'path': 'b.py', 'old': 'x = 1', 'new': 'x = 2'}])
        self.assertEqual(reply['disputes'], {'Brief.test_odd': '"Return twice x."'})
        files = workflow.materialize_text({'a.py': '', 'b.py': 'x = 1\ny = 0\n'}, {'a.py': '', 'b.py': ''}, reply)
        self.assertEqual(files['b.py'], 'x = 2\ny = 0\n')

    def test_an_end_line_after_a_file_block_closes_it(self):
        reply = workflow.parse_text_reply('@@@ FILE a.py\nx = 1\n@@@ END\n@@@ FILE b.py\ny = 2\n@@@ END\n')
        self.assertEqual(reply['files'], {'a.py': 'x = 1\n', 'b.py': 'y = 2\n'})

    def test_a_fenced_reply_and_a_json_reply_are_both_read(self):
        self.assertEqual(workflow.parse_text_reply('```\n@@@ FILE a.py\nx\n```')['files'], {'a.py': 'x\n'})
        reply = workflow.parse_text_reply(json.dumps({'files': {'a.py': 'y\n'}, 'rationale': 'r'}))
        self.assertEqual((reply['files'], reply['rationale']), ({'a.py': 'y\n'}, 'r'))

    def test_a_bare_marker_is_with_after_a_replace_and_end_elsewhere(self):
        reply = workflow.parse_text_reply('@@@ REPLACE a.py\nx = 1\n@@@\nx = 2\n@@@ END\n@@@ FILE b.py\ny\n@@@\n')
        self.assertEqual(reply['edits'], [{'path': 'a.py', 'old': 'x = 1', 'new': 'x = 2'}])
        self.assertEqual(reply['files'], {'b.py': 'y\n'})

    def test_a_four_quote_docstring_suite_still_loads(self):
        q4 = '"' * 4
        source, ids = workflow.spec_test_file('```python\nimport unittest\nclass T(unittest.TestCase):\n'
                                              f'    def test_a(self):\n        {q4}Say it.{q4}\n        pass\n```')
        self.assertEqual(ids, ['T.test_a'])
        compile(source, 'spec', 'exec')

    def test_malformed_replies_are_refused(self):
        for text in ('no blocks at all', '@@@ REPLACE a.py\nx\n@@@ END\n', '@@@ SHELL rm -rf /\n'):
            with self.assertRaises(ValueError, msg=text):
                workflow.parse_text_reply(text)
        reply = workflow.parse_text_reply('@@@ FILE other.py\nx\n')
        with self.assertRaisesRegex(ValueError, 'not a declared'):
            workflow.materialize_text({'a.py': ''}, {'a.py': ''}, reply)
        both = workflow.parse_text_reply('@@@ FILE a.py\nx\n@@@ REPLACE a.py\nx\n@@@ WITH\ny\n@@@ END\n')
        with self.assertRaisesRegex(ValueError, 'both'):
            workflow.materialize_text({'a.py': 'x\n'}, {'a.py': ''}, both)


class RoleTest(unittest.TestCase):
    def test_codex_gets_model_and_effort_before_its_stdin_marker(self):
        backend = workflow.role_backend(harness.backend('codex-chatgpt'), {'model': 'gpt-6-luna', 'effort': 'low'})
        self.assertEqual(backend['argv'][-5:], ['--model', 'gpt-6-luna', '-c', 'model_reasoning_effort="low"', '-'])

    def test_claude_takes_a_model_but_not_an_effort(self):
        backend = workflow.role_backend(harness.backend('claude'), {'model': 'opus'})
        self.assertEqual(backend['argv'][backend['argv'].index('--model') + 1], 'opus')
        with self.assertRaises(ValueError):
            workflow.role_backend(harness.backend('claude'), {'effort': 'high'})


class SpecTestLoopTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.spec = {'version': 1, 'brief': 'Implement f(x).\n\nReturn twice x.\n', 'files': {'value.py': ''},
                     'fixtures': {'check.py': "from value import f\nassert f(5) == 10\n"},
                     'checks': [{'name': 'reference', 'argv': [sys.executable, '-B', 'check.py']}],
                     'writer': 'fake', 'judge': 'fake', 'allow_tool_backends': True, 'allow_execution': True,
                     'max_rounds': 3, 'max_seconds': 60, 'protocol': 'text', 'spec_tests': {'python': sys.executable}}

    def run_with(self, replies, tests=SPEC_TESTS, reviews=(REVIEW_OK,), **changes):
        path = self.root / 'task.json'
        path.write_text(json.dumps({**self.spec, **changes}))
        seen, writer, reviews = [], iter(replies), iter(reviews)

        def call(backend, system, message, timeout_s):
            seen.append((system, message, backend))
            if system.startswith('Write a Python unittest module'):
                return tests, {}
            if system.startswith('Implement the brief') or system.startswith('Write the document'):
                return next(writer), {}
            return next(reviews), {}
        return workflow.run(path, self.root / 'run', call_fn=call), seen

    def test_a_failing_spec_test_drives_a_revision_before_the_review(self):
        report, seen = self.run_with(['@@@ FILE value.py\ndef f(x):\n    return int(2*x)\n',
                                      '@@@ REPLACE value.py\n    return int(2*x)\n@@@ WITH\n    return 2*x\n@@@ END\n'])
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(report['usage']['calls'], 4)  # spec tests, writer, writer, review
        self.assertEqual(report['rounds'][0]['spec_tests']['failing_count'], 1)
        self.assertIn('Brief.test_keeps_floats', seen[2][1])  # the writer sees the failing test and its code
        self.assertIn('assertIsInstance(f(0.5), float)', seen[2][1])
        self.assertNotIn('return int', seen[0][1])  # the test writer never sees a candidate
        self.assertEqual((self.root / 'run/final/value.py').read_text(), 'def f(x):\n    return 2*x\n')

    def test_a_disputed_test_stops_gating_and_the_same_candidate_goes_to_review(self):
        wrong = SPEC_TESTS.replace('self.assertEqual(f(5), 10)', 'self.assertEqual(f(5), 11)')
        good = '@@@ FILE value.py\ndef f(x):\n    return 2*x\n'
        report, seen = self.run_with([good, '@@@ DISPUTE Brief.test_doubles_integers\n"Return twice x."\n'], wrong)
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual([r['status'] for r in report['rounds']], ['revise', 'ready_for_review'])
        self.assertIn('disputed by the writer: Brief.test_doubles_integers', seen[-1][0])

    def test_an_unchanged_reply_to_spec_failures_disputes_them_and_reaches_the_review(self):
        wrong = SPEC_TESTS.replace('self.assertEqual(f(5), 10)', 'self.assertEqual(f(5), 11)')
        good = '@@@ FILE value.py\ndef f(x):\n    return 2*x\n'
        report, seen = self.run_with([good, good], wrong)
        self.assertEqual([r['status'] for r in report['rounds']], ['revise', 'ready_for_review'])
        self.assertIn('the writer left the code unchanged', seen[-1][0] + seen[-1][1])

    def test_a_spec_test_every_candidate_fails_stops_gating(self):
        wrong = SPEC_TESTS.replace('self.assertEqual(f(5), 10)', 'self.assertEqual(f(5), 11)')
        a, b = '@@@ FILE value.py\ndef f(x):\n    return 2*x\n', '@@@ FILE value.py\ndef f(x):\n    return x*2\n'
        report, seen = self.run_with([a, b], wrong, candidates=2)
        self.assertEqual([r['status'] for r in report['rounds']], ['ready_for_review'])
        self.assertEqual(report['rounds'][0]['agreement_disputed'], ['Brief.test_doubles_integers'])
        self.assertEqual(report['usage']['calls'], 4)  # spec tests, two writers, review: no revision for a suspect test

    def test_a_revision_that_breaks_a_passing_spec_test_is_set_aside(self):
        three = SPEC_TESTS[:-3].rstrip() + '\n\n    def test_negative(self):\n        self.assertEqual(f(-2), -4)\n```'
        first = '@@@ FILE value.py\ndef f(x):\n    return int(2*x)\n'           # fails floats only
        breaks = '@@@ FILE value.py\ndef f(x):\n    return 2*abs(x) if x != 5 else 10\n'  # fixes floats, breaks negatives
        good = '@@@ FILE value.py\ndef f(x):\n    return 2*x\n'
        report, seen = self.run_with([first, breaks, good], three, no_regression=True)
        self.assertEqual([r['status'] for r in report['rounds']], ['revise', 'rejected', 'ready_for_review'])
        self.assertEqual(report['rounds'][1]['rejected_for'], ['Brief.test_negative'])
        self.assertTrue((self.root / 'run/round-2/rejected').is_dir())
        self.assertFalse((self.root / 'run/round-2/candidate').exists())
        writer3 = [m for system, m, _ in seen if system.startswith('Implement the brief')][2]
        self.assertIn('set aside', writer3)
        self.assertIn('return int(2*x)', writer3)  # the earlier code comes back
        self.assertIn('Brief.test_keeps_floats', writer3)  # with the feedback the revision was answering
        self.assertEqual((self.root / 'run/final/value.py').read_text(), 'def f(x):\n    return 2*x\n')

    def test_a_revision_that_fails_the_checks_is_set_aside_with_no_regression(self):
        good = '@@@ FILE value.py\ndef f(x):\n    return 2*x\n'
        report, seen = self.run_with([good, '@@@ FILE value.py\ndef f(x):\n    return x\n', good.replace('2*x', 'x*2')],
                                     reviews=('### 1. Name (`value.py:2`)\n- Problem: rename.\n- Fix: rename.', REVIEW_OK),
                                     no_regression=True)
        self.assertEqual([r['status'] for r in report['rounds']], ['revise', 'rejected', 'ready_for_review'])
        self.assertEqual(report['rounds'][1]['rejected_for'], 'checks')

    def test_script_only_candidates_run_the_script_and_go_on(self):
        doc = {'files': {'REPORT.md': ''}, 'fixtures': {'check.py': "assert 'total' in open('REPORT.md').read()\n"},
               'spec_tests': None, 'scripts': {'python': sys.executable}, 'candidates': 2}
        only = '@@@ SCRIPT\nprint("total", 6)\n'
        report, seen = self.run_with([only, only, '@@@ FILE REPORT.md\ntotal: 6\n'], **doc)
        self.assertEqual([r['status'] for r in report['rounds']], ['script', 'ready_for_review'])
        self.assertIn('total 6', seen[2][1])

    def test_a_rationale_only_reply_stands_by_the_code(self):
        good = '@@@ FILE value.py\ndef f(x):\n    return 2*x\n'
        report, _ = self.run_with([good, '@@@ RATIONALE\nThe review is mistaken: f already doubles x.\n'],
                                  reviews=('### 1. Name (`value.py:2`)\n- Problem: rename.\n- Fix: rename.', REVIEW_OK))
        self.assertEqual([r['status'] for r in report['rounds']], ['revise', 'ready_for_review'])

    def test_a_suite_that_cannot_load_is_dropped(self):
        report, _ = self.run_with(['@@@ FILE value.py\ndef f(x):\n    return 2*x\n'],
                                  '```python\nfrom value import g\n\nimport unittest\nclass T(unittest.TestCase):\n'
                                  '    def test_x(self):\n        pass\n```')
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertIn('dropped', report['spec_tests'])
        self.assertEqual(report['usage']['calls'], 3)

    def test_documents_use_the_document_prompts_and_no_spec_tests(self):
        doc = {'files': {'REPORT.md': ''}, 'fixtures': {'check.py': "assert 'x' in open('REPORT.md').read()\n"}}
        report, seen = self.run_with(['@@@ FILE REPORT.md\nx = 2\n'], **doc)
        self.assertEqual(report['artifact_kind'], 'doc')
        self.assertTrue(seen[0][0].startswith('Write the document'))
        self.assertTrue(seen[1][0].startswith('You review documents against their sources.'))
        self.assertEqual(report['usage']['calls'], 2)

    def test_escalation_raises_effort_only_after_a_failed_check(self):
        roles = {'writer': {'effort': 'low'}, 'revision': {'effort': 'low'}, 'escalate': {'effort': 'high'}}
        report, seen = self.run_with(['@@@ FILE value.py\ndef f(x):\n    return x\n',
                                      '@@@ FILE value.py\ndef f(x):\n    return 2*x\n'],
                                     writer='codex-chatgpt', judge='codex-chatgpt', roles=roles,
                                     allow_tool_backends=False)
        self.assertEqual([r['role'] for r in report['rounds']], ['writer', 'escalate'])
        writers = [b['argv'] for system, _, b in seen if system.startswith('Implement the brief')]
        self.assertIn('model_reasoning_effort="low"', writers[0])
        self.assertIn('model_reasoning_effort="high"', writers[1])


    def test_candidates_draw_k_writers_in_round_one_and_keep_the_one_that_passes(self):
        good, bad = '@@@ FILE value.py\ndef f(x):\n    return 2*x\n', '@@@ FILE value.py\ndef f(x):\n    return x\n'
        report, seen = self.run_with([bad, good, bad], candidates=3)
        self.assertEqual(report['status'], 'ready_for_review')
        first = report['rounds'][0]
        self.assertEqual(sorted(x['checks_pass'] for x in first['samples'] if 'checks_pass' in x), [False, True])
        self.assertEqual(len(first['samples']), 2)  # the two equal replies are evaluated once
        self.assertEqual((self.root / 'run/round-1/candidate/value.py').read_text(), 'def f(x):\n    return 2*x\n')
        # spec tests + 3 writers + review: the bound is (max_rounds - 1) + candidates writer calls
        self.assertEqual(report['usage']['calls'], 5)
        self.assertEqual(sum(system.startswith('Implement the brief') for system, _, _ in seen), 3)

    def test_candidates_never_exceed_their_bound(self):
        bad = ['@@@ FILE value.py\ndef f(x):\n    return x + %d\n' % i for i in range(6)]
        report, seen = self.run_with(bad, candidates=2)
        self.assertEqual(report['status'], 'needs_revision')
        self.assertEqual(sum(system.startswith('Implement the brief') for system, _, _ in seen), 2 + 2)
        self.assertEqual(report['usage']['calls'], 1 + 4)  # spec tests + (max_rounds - 1) + candidates

    def test_a_script_runs_and_its_output_reaches_the_writer_and_the_review(self):
        doc = {'files': {'REPORT.md': ''}, 'fixtures': {'data.csv': 'a,b\n1,2\n3,4\n',
                                                         'check.py': "assert 'total' in open('REPORT.md').read()\n"},
               'spec_tests': None, 'scripts': {'python': sys.executable}}
        script = ('@@@ SCRIPT\nimport csv\nrows = list(csv.DictReader(open("data.csv")))\n'
                  'print("total", sum(int(r["b"]) for r in rows))\n@@@ FILE REPORT.md\ntotal: 5\n')
        fixed = '@@@ REPLACE REPORT.md\ntotal: 5\n@@@ WITH\ntotal: 6\n@@@ END\n'
        report, seen = self.run_with([script, fixed], **doc)
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertTrue(seen[0][0].startswith('Write the document') and 'SCRIPT' in seen[0][0])
        self.assertEqual(report['rounds'][0]['script']['status'], 'pass')
        self.assertIn('total 6', seen[1][1])  # the writer sees the output before it finishes
        self.assertIn("the writer's script", seen[2][0] + seen[2][1])  # and so does the review
        self.assertEqual(report['usage']['calls'], 3)  # writer, writer, review; the script is not a model call
        self.assertEqual((self.root / 'run/final/REPORT.md').read_text(), 'total: 6\n')
        self.assertFalse((self.root / 'run/final/_writer_script.py').exists())

    def test_protocol_repair_spends_one_call_not_the_round(self):
        bad = '@@@ REPLACE value.py\nnot there\n@@@ WITH\nx\n@@@ END\n'
        good = '@@@ FILE value.py\ndef f(x):\n    return 2*x\n'
        report, seen = self.run_with([bad, good], protocol_repair=True)
        self.assertEqual(report['status'], 'ready_for_review')
        self.assertEqual(len(report['rounds']), 1)
        self.assertIn('repair', report['rounds'][0])
        self.assertIn('could not be applied', seen[2][1])  # the repair call sees the exact error
        self.assertIn('complete FILE block', seen[2][1])  # and is asked for whole files, which cannot mis-anchor
        self.assertEqual(report['usage']['calls'], 4)  # spec tests, writer, repair, review

    def test_script_and_candidate_options_are_validated(self):
        base = {**self.spec, 'spec_tests': None}
        for changes, message in (({'candidates': 5}, 'candidates'), ({'candidates': 0}, 'candidates'),
                                 ({'scripts': {'python': sys.executable}, 'protocol': 'json'}, 'text protocol'),
                                 ({'scripts': {}}, 'python executable')):
            path = self.root / 'bad.json'
            path.write_text(json.dumps({k: v for k, v in {**base, **changes}.items() if v is not None}))
            with self.assertRaisesRegex(ValueError, message):
                workflow.load_spec(path)
        with self.assertRaisesRegex(ValueError, 'at most one script'):
            workflow.parse_text_reply('@@@ SCRIPT\nprint(1)\n@@@ SCRIPT\nprint(2)\n')


if __name__ == '__main__':
    unittest.main()
