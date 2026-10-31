"""Offline checks of findings.py and review --findings-json: temporary git repositories, fake backends only."""
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import backends  # noqa: E402
import findings  # noqa: E402
import harness  # noqa: E402

ENV = {**os.environ, 'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_AUTHOR_NAME': 't',
       'GIT_AUTHOR_EMAIL': 't@t', 'GIT_COMMITTER_NAME': 't', 'GIT_COMMITTER_EMAIL': 't@t'}

REVIEW = '''What this change does: x.

### 1. Cache is read without the lock
- Location: `src/pool.py:4-6`
- Severity: High
- Category: concurrency
- Code: `conns = self.conns`
- Problem: two threads call get() at once;
  both read the same connection.
  - Nested detail stays.
- Fix: hold the lock.
```
### 6. Not a finding inside a fence (`fenced.py:1`)
```

### 2. Workflow and docs disagree (`.github/workflows/ci.yml:~12`, `docs/guide.md:3-9`; `:20-22`)
- Code: `x`
- Problem: the docs say y.

### 3. `lsmysql.New` panics on nil (`README.md`)
- Problem: z.

### 4. Off by one in `d.db` handling
Body text mentions `d.db` first, then `./src/pool.py:2` where it happens.

### 5. Nothing to anchor
- Problem: no file at all.

### Open questions
### 7. Numbered question (`q.py:1`)

### Not checked
- `n.py:3`
'''


def git(repo, *args):
    return subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True, text=True, env=ENV).stdout


def make_repo(d):
    repo = Path(d) / 'repo'
    for path, text in {'src/pool.py': 'a\nb\nc\nd\ne\nf\ng\n', 'README.md': '# R\n',
                       '.github/workflows/ci.yml': 'on: push\n', 'bin.dat': '\0\0',
                       'docs/SKILL.md': 'd\n', 'skills/SKILL.md': 's\n'}.items():
        (repo / path).parent.mkdir(parents=True, exist_ok=True)
        (repo / path).write_text(text)
    git(repo, 'init', '-q')
    git(repo, 'add', '-A')
    git(repo, '-c', 'commit.gpgsign=false', 'commit', '-qm', 'base')
    base = git(repo, 'rev-parse', 'HEAD').strip()
    (repo / 'src/pool.py').write_text('a\nb\nc\nd\ne\nf\ng\nh\n')
    (repo / 'skills/SKILL.md').write_text('s\nt\n')
    git(repo, '-c', 'commit.gpgsign=false', 'commit', '-qam', 'head')
    return repo, base, git(repo, 'rev-parse', 'HEAD').strip()


class ParseTest(unittest.TestCase):
    def test_locations_come_from_the_location_line_heading_or_body(self):
        got = [(f['finding'], f['file'], f['start_line'], f['end_line']) for f in findings.parse(REVIEW)]
        self.assertEqual(got, [(1, 'src/pool.py', 4, 6), (2, '.github/workflows/ci.yml', 12, 12),
                               (2, 'docs/guide.md', 3, 9), (3, 'README.md', 1, 1), (4, 'src/pool.py', 2, 2)])

    def test_message_is_title_and_problem_with_optional_severity_and_category(self):
        first, second = findings.parse(REVIEW)[:2]
        self.assertEqual(first['message'], 'Cache is read without the lock. two threads call get() at once; both read '
                                           'the same connection. - Nested detail stays.')
        self.assertEqual((first['severity'], first['category']), ('high', 'concurrency'))
        self.assertNotIn('severity', second)
        self.assertTrue(second['message'].endswith('. the docs say y.'))
        body_only = findings.parse('### 1. T (`a.py:3`)\nJust prose ' + 'x' * 3000)[0]
        self.assertEqual(len(body_only['message']), findings.MESSAGE)
        self.assertTrue(body_only['message'].startswith('T (`a.py:3`). Just prose'))

    def test_no_findings_and_terminal_sections(self):
        self.assertEqual(findings.parse('What this change does: x.\n\nNo defects found.\n\n### Not checked\n- `a.py:1`'), [])
        files = {f['file'] for f in findings.parse(REVIEW)}
        self.assertFalse(files & {'fenced.py', 'q.py', 'n.py'})
        self.assertEqual(max(f['finding'] for f in findings.parse(REVIEW)), 4)  # 5 has no file; 6 is fenced

    def test_paths_are_normalized_and_lines_ordered(self):
        got = findings.parse('### 1. T (`.\\src\\a.py:9-3`, `/b/c.ts#L4-L5`, `Makefile:2`, `x.py:L7`)\n- Problem: p')
        self.assertEqual([(f['file'], f['start_line'], f['end_line']) for f in got],
                         [('src/a.py', 9, 9), ('b/c.ts', 4, 5), ('Makefile', 2, 2), ('x.py', 7, 7)])


class AnchorTest(unittest.TestCase):
    def test_missing_files_are_dropped_and_lines_clamped_at_head(self):
        with tempfile.TemporaryDirectory() as d:
            repo, base, head = make_repo(d)
            items = [{'file': f, 'start_line': a, 'end_line': b, 'message': 'm'} for f, a, b in [
                ('src/pool.py', 7, 99), ('docs/guide.md', 1, 1), ('a/README.md', 5, 3), ('pool.py', 0, 2),
                ('ci.yml', 1, 1), ('bin.dat', 4, 9)]]
            kept, dropped = findings.anchor(items, repo, head[:10])
            self.assertEqual([(f['file'], f['start_line'], f['end_line']) for f in kept],
                             [('src/pool.py', 7, 8), ('README.md', 1, 1), ('src/pool.py', 1, 2),
                              ('.github/workflows/ci.yml', 1, 1), ('bin.dat', 1, 1)])
            self.assertEqual([f['file'] for f in dropped], ['docs/guide.md'])
            # The base commit has one line fewer: lines refer to the file at the commit given.
            self.assertEqual(findings.anchor(items[:1], repo, base)[0][0]['end_line'], 7)
            with self.assertRaises(ValueError):
                findings.anchor(items, repo, '--output=x')
            # A name several files share is ambiguous, unless the base says which one of them changed.
            skill = [{'file': 'SKILL.md', 'start_line': 2, 'end_line': 2, 'message': 'm'}]
            self.assertEqual(findings.anchor(skill, repo, head)[1], skill)
            self.assertEqual(findings.anchor(skill, repo, head, base)[0][0]['file'], 'skills/SKILL.md')

    def test_contract_json_shape(self):
        data = findings.reviewbench(findings.parse(REVIEW)[:1], {'repo': 'https://github.com/o/r', 'pr_number': '12',
                                                                'base': 'b' * 40, 'head': 'h' * 40, 'title': 't'}, 'me')
        self.assertEqual(data, {'pr': {'repo': 'https://github.com/o/r', 'pr_number': 12, 'base': 'b' * 40,
                                       'head': 'h' * 40}, 'agent': 'me',
                                'findings': [{'file': 'src/pool.py', 'start_line': 4, 'end_line': 6,
                                              'message': data['findings'][0]['message'], 'producer': 'me'}]})
        self.assertEqual(findings.reviewbench([], {}, 'x')['pr'], dict.fromkeys(('repo', 'pr_number', 'base', 'head')))


class CliTest(unittest.TestCase):
    def run_cli(self, argv):
        with mock.patch.object(backends, 'run_isolated', return_value=REVIEW) as run, \
                mock.patch('sys.stderr', new_callable=io.StringIO) as err, \
                mock.patch('sys.stdout', new_callable=io.StringIO):
            code = harness.main(argv)
        return code, err.getvalue(), run.call_count

    def test_review_repo_writes_anchored_contract_json(self):
        with tempfile.TemporaryDirectory() as d:
            repo, base, head = make_repo(d)
            pr = Path(d, 'pr.json')
            pr.write_text(json.dumps({'repo': 'https://github.com/o/r', 'pr_number': 7, 'base': base, 'head': head,
                                      'title': 't', 'body': None}))
            out, trace = Path(d, 'out.json'), Path(d, 'trace')
            code, err, calls = self.run_cli(['review', '--repo', str(repo), '--base', base, '--head', head[:12],
                                             '--backend', 'finder-cmd', '--findings-json', str(out), '--rb-pr-json',
                                             str(pr), '--agent', 'rb-agent', '--trace', str(trace)])
            self.assertEqual((code, calls), (0, 1), err)
            data = json.loads(out.read_text())
            self.assertEqual(data['pr'], {'repo': 'https://github.com/o/r', 'pr_number': 7, 'base': base, 'head': head})
            self.assertEqual(data['agent'], 'rb-agent')
            self.assertEqual([(f['file'], f['start_line'], f['end_line'], f['producer']) for f in data['findings']],
                             [('src/pool.py', 4, 6, 'rb-agent'), ('.github/workflows/ci.yml', 1, 1, 'rb-agent'),
                              ('README.md', 1, 1, 'rb-agent'), ('src/pool.py', 2, 2, 'rb-agent')])
            self.assertEqual(json.loads((trace / 'findings.json').read_text()), data)
            summary = json.loads(err.strip().splitlines()[-1].split('harness: ', 1)[1])
            self.assertEqual((summary['findings_written'], summary['findings_dropped']), (4, 1))

    def test_input_review_writes_unchecked_findings_and_bad_pr_json_stops_before_any_call(self):
        with tempfile.TemporaryDirectory() as d:
            m, out = Path(d, 'm.txt'), Path(d, 'out.json')
            m.write_text('M')
            code, err, _ = self.run_cli(['review', '--input', str(m), '--backend', 'x', '--findings-json', str(out)])
            self.assertEqual(code, 0, err)
            data = json.loads(out.read_text())
            self.assertEqual((data['agent'], data['pr']['head'], len(data['findings'])), ('ringo', None, 5))
            repo, base, head = make_repo(d)
            pr = Path(d, 'pr.json')
            pr.write_text(json.dumps({'pr_number': 1, 'head': base}))  # names another commit than --head
            code, err, calls = self.run_cli(['review', '--repo', str(repo), '--base', base, '--head', head,
                                             '--backend', 'x', '--findings-json', str(out), '--rb-pr-json', str(pr)])
            self.assertEqual((code, calls), (2, 0))
            self.assertIn('cannot start', err)
            with mock.patch('sys.stderr', new_callable=io.StringIO), self.assertRaises(SystemExit):
                harness.main(['review', '--input', str(m), '--rb-pr-json', str(pr)])



ENTRYPOINT = Path(__file__).resolve().parent.parent / 'adapters' / 'reviewbench' / 'entrypoint.sh'


@unittest.skipUnless(shutil.which('bash'), 'needs bash')
@unittest.skipIf(os.name == 'nt', 'the ReviewBench entrypoint is a bash script for its Linux container image')
class AdapterTest(unittest.TestCase):
    """The ReviewBench entrypoint, run outside a container against a fake `claude` CLI, with a private HOME and
    git config so its `git config --global` never touches the real ones."""

    def run_adapter(self, d, **config):
        repo, base, head = make_repo(d)
        bin_dir, home = Path(d, 'bin'), Path(d, 'home')
        bin_dir.mkdir()
        home.mkdir()
        fake = bin_dir / 'claude'
        fake.write_text(f'#!{sys.executable}\nimport json, sys\nsys.stdin.read()\n'
                        f'open({str(Path(d, "argv.json"))!r}, "w").write(json.dumps(sys.argv))\n'
                        f'print(json.dumps({{"result": {REVIEW!r}, "total_cost_usd": 0.01}}))\n')
        fake.chmod(0o755)
        pr = Path(d, 'pr.json')
        pr.write_text(json.dumps({'repo': 'https://github.com/o/r', 'pr_number': 3, 'base': base, 'head': head,
                                  'nwo': 'o/r', 'title': 'Fix pool', 'body': None}))
        env = {**ENV, 'PATH': f'{bin_dir}{os.pathsep}{os.environ["PATH"]}', 'HOME': str(home),
               'GIT_CONFIG_GLOBAL': str(home / '.gitconfig'), 'RINGO_HOME': str(ENTRYPOINT.parents[2]),
               'RB_REPO': str(repo), 'RB_BASE': base, 'RB_HEAD': head, 'RB_AGENT': 'ringo-rb',
               'RB_PR_JSON': str(pr), 'RB_OUT': str(Path(d, 'findings.json')), **config}
        proc = subprocess.run(['bash', str(ENTRYPOINT)], env=env, capture_output=True, text=True, timeout=120)
        return proc, head

    def test_entrypoint_writes_the_contract_file_and_prints_its_settings(self):
        with tempfile.TemporaryDirectory() as d:
            proc, head = self.run_adapter(d, RB_CONFIG_MODEL='claude-sonnet-5')
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn('model=claude-sonnet-5', proc.stdout)
            argv = json.loads(Path(d, 'argv.json').read_text())
            self.assertEqual(argv[argv.index('--model') + 1], 'claude-sonnet-5')
            data = json.loads(Path(d, 'findings.json').read_text())
            self.assertEqual((data['pr']['head'], data['pr']['pr_number'], data['agent']), (head, 3, 'ringo-rb'))
            self.assertEqual(len(data['findings']), 4)

    def test_entrypoint_failures_exit_non_zero_without_a_findings_file(self):
        with tempfile.TemporaryDirectory() as d:
            proc, _ = self.run_adapter(d, RB_CONFIG_BACKEND='codex', RB_CONFIG_MODEL='m')
            self.assertEqual(proc.returncode, 2)
            self.assertIn('RB_CONFIG_MODEL works only with', proc.stderr)
        with tempfile.TemporaryDirectory() as d:
            proc, _ = self.run_adapter(d, RB_CONFIG_BACKEND='false')
            self.assertNotEqual(proc.returncode, 0)
            self.assertFalse(Path(d, 'findings.json').exists())


if __name__ == '__main__':
    unittest.main()
