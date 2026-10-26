"""Offline checks of context.py: temporary git repositories made in the test, no network, no model calls."""
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import backends  # noqa: E402
import context  # noqa: E402
import harness  # noqa: E402

ENV = {**os.environ, 'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_AUTHOR_NAME': 't',
       'GIT_AUTHOR_EMAIL': 't@t', 'GIT_COMMITTER_NAME': 't', 'GIT_COMMITTER_EMAIL': 't@t'}

BASE = {
    'a.py': 'LIMIT = 3\n\n\nclass Shape:\n    def area(self, x):\n        return x * LIMIT\n\n\n'
            'def scale(x):\n    return x + 1\n\n\ndef retired(x):\n    return x\n',
    'callers.py': 'from a import scale\n\n\ndef grow():\n    return scale(2)\n',
    'square.py': 'from a import Shape\n\n\nclass Square(Shape):\n    pass\n',
    'tests/test_a.py': 'from a import scale\n\n\ndef test_scale():\n    assert scale(1) == 2\n',
    'old_user.py': 'from a import retired\n\nVALUE = retired(1)\n',
    'unrelated.py': 'def nothing():\n    return None\n',
    'web/cart.js': 'function computeTotal(items) {\n  return items.length;\n}\n',
    'web/page.js': 'const n = computeTotal([1, 2]);\n',
    'README.md': '# Demo\n',
}


def git(repo, *args):
    return subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True, text=True, env=ENV).stdout


def make_repo(d, extra=None):
    """A repository with a base and a head commit: head changes scale, Shape.area and computeTotal, and deletes
    retired. Returns (repo, base sha, head sha)."""
    repo = Path(d) / 'repo'
    repo.mkdir()
    git(repo, 'init', '-q')
    for path, text in {**BASE, **(extra or {})}.items():
        (repo / path).parent.mkdir(parents=True, exist_ok=True)
        (repo / path).write_text(text)
    git(repo, 'add', '-A')
    git(repo, '-c', 'commit.gpgsign=false', 'commit', '-qm', 'base')
    base = git(repo, 'rev-parse', 'HEAD').strip()
    a = (repo / 'a.py').read_text().replace('x + 1', 'x + 2').replace('x * LIMIT', 'x * LIMIT * 2')
    (repo / 'a.py').write_text(a.split('\n\ndef retired')[0] + '\n')
    (repo / 'web/cart.js').write_text('function computeTotal(items) {\n  return items.length + 1;\n}\n')
    (repo / 'NOTES.md').write_text('The function unrelated_prose is not a symbol.\n')
    git(repo, 'add', 'NOTES.md')
    git(repo, '-c', 'commit.gpgsign=false', 'commit', '-qam', 'head')
    return repo, base, git(repo, 'rev-parse', 'HEAD').strip()


def blocks(material):
    return [line[4:-len(' (unchanged, at head) ===')] for line in material.splitlines()
            if line.startswith('=== ') and line.endswith(' (unchanged, at head) ===')]


class ContextTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo, self.base, self.head = make_repo(self.tmp.name)

    def test_material_has_the_change_then_callers_subclasses_and_tests_with_a_manifest(self):
        m = context.build(self.repo, self.base, self.head, pr_text='Bump scale')
        order = ['=== PR description ===\nBump scale', '=== change.diff ===', '=== a.py (after the change) ===',
                 '=== web/cart.js (after the change) ===', '=== README.md ===', '=== context manifest ===']
        self.assertEqual([m.index(x) for x in order], sorted(m.index(x) for x in order))
        self.assertIn('-    return x + 1\n+    return x + 2', m)
        # Callers of a changed function (Python and JS), the caller of a deleted one, a subclass of the class
        # whose method changed; the test comes last, among its kind; nothing unrelated.
        self.assertEqual(blocks(m), ['square.py', 'old_user.py', 'web/page.js', 'callers.py', 'tests/test_a.py'])
        self.assertIn('=== callers.py (unchanged, at head) ===\nfrom a import scale\n', m)
        self.assertNotIn('unrelated.py', m)
        manifest = m.split('=== context manifest ===')[1]
        self.assertIn(f'base {self.base}, head {self.head}', manifest)
        self.assertIn('(4): area, computeTotal, retired, scale.', manifest)
        self.assertIn('searched for subclasses only (1): Shape.', manifest)
        self.assertIn('- square.py: whole file; subclasses or implements Shape', manifest)
        self.assertIn('- tests/test_a.py: whole file; tests or uses scale', manifest)
        self.assertNotIn(str(self.repo), m)  # the local path is not sent to the provider

    def test_the_same_commits_give_the_same_bytes_and_nothing_is_written(self):
        status = git(self.repo, 'status', '--porcelain', '--untracked-files=all')
        objects = git(self.repo, 'count-objects', '-v')
        (self.repo / 'callers.py').write_text('def grow():\n    return scale(99)  # UNCOMMITTED\n')
        (self.repo / 'scratch.py').write_text('scale(3)  # UNTRACKED\n')
        first = context.build(self.repo, self.base, self.head)
        self.assertEqual(first, context.build(str(self.repo), self.base[:12], self.head))
        self.assertNotIn('UNCOMMITTED', first)  # git objects at head, never the work tree
        self.assertNotIn('UNTRACKED', first)
        self.assertEqual(git(self.repo, 'count-objects', '-v'), objects)
        self.assertNotEqual(status, git(self.repo, 'status', '--porcelain', '--untracked-files=all'))  # only our edits
        git(self.repo, 'checkout', '-q', '--', 'callers.py')
        (self.repo / 'scratch.py').unlink()
        self.assertEqual(status, git(self.repo, 'status', '--porcelain', '--untracked-files=all'))

    def test_the_budget_limits_context_only_and_names_what_it_left_out(self):
        full = context.build(self.repo, self.base, self.head)
        cut = len(full.encode()) - 120
        m = context.build(self.repo, self.base, self.head, budget=cut)
        self.assertLessEqual(len(m.encode()), cut)
        self.assertLess(len(blocks(m)), len(blocks(full)))
        dropped = [b for b in blocks(full) if b not in blocks(m)]
        manifest = m.split('=== context manifest ===')[1].split('Left out as binary')[0]
        for path in dropped:
            self.assertIn(f'- {path}: ', manifest.split('Left out for the byte budget:')[1])
        # A budget smaller than the change keeps the whole change and adds no context.
        tiny = context.build(self.repo, self.base, self.head, budget=10)
        self.assertEqual(blocks(tiny), [])
        for part in ('=== change.diff ===', '=== a.py (after the change) ===', '=== web/cart.js (after the change) ==='):
            self.assertIn(part, tiny)
        self.assertIn('- callers.py: refers to scale', tiny)
        for bad in (0, -1, 1.5):
            with self.assertRaises(context.ContextError):
                context.build(self.repo, self.base, self.head, budget=bad)

    def test_a_long_left_out_list_is_counted_when_it_alone_would_break_the_budget(self):
        extra = {f'pkg/{"long_directory_name_" * 3}{i}/user_{i}.py': 'from a import scale\nV = scale(1)\n'
                 for i in range(150)}
        with tempfile.TemporaryDirectory() as d:
            repo, base, head = make_repo(d, extra)
            required = context.build(repo, base, head, budget=10).split('\n=== context manifest ===')[0]
            budget = len(required.encode()) + 2500
            m = context.build(repo, base, head, budget=budget)
        self.assertLessEqual(len(m.encode()), budget)
        self.assertIn('unchanged file(s) that reference the change', m)

    def test_a_large_referencing_file_comes_as_numbered_windows(self):
        big = ''.join(f'filler_{i} = {i}\n' for i in range(2000)) + 'TOTAL = scale(5)\n'
        with tempfile.TemporaryDirectory() as d:
            repo, base, head = make_repo(d, {'big.py': big})
            m = context.build(repo, base, head)
        block = m.split('=== big.py (unchanged, at head) ===\n')[1].split('\n=== ')[0]
        self.assertTrue(block.startswith('--- lines 1986-2001 ---\n1986: filler_1985 = 1985\n'))
        self.assertIn('2001: TOTAL = scale(5)', block)
        self.assertIn('- big.py: lines 1986-2001; refers to scale', m)

    def test_windows_number_lines_as_git_does_despite_form_feeds(self):
        big = 'x = 1\n' + '\x0c' * 40 + '\n' + ''.join(f'filler_{i} = {i}\n' for i in range(2000)) + 'TOTAL = scale(5)\n'
        with tempfile.TemporaryDirectory() as d:
            repo, base, head = make_repo(d, {'big.py': big})
            m = context.build(repo, base, head)
        block = m.split('=== big.py (unchanged, at head) ===\n')[1].split('\n=== ')[0]
        self.assertTrue(block.startswith('--- lines 1988-2003 ---\n'), block[:80])
        self.assertIn('2003: TOTAL = scale(5)', block)

    def test_changed_paths_are_literal_not_globs(self):
        with tempfile.TemporaryDirectory() as d:
            repo = Path(d) / 'repo'
            repo.mkdir()
            git(repo, 'init', '-q')
            (repo / '[ab].py').write_text('one = 1\ntwo = 2\n')
            (repo / 'a.py').write_text(''.join(f'v{i} = {i}\n' for i in range(40)))
            git(repo, 'add', '-A')
            git(repo, '-c', 'commit.gpgsign=false', 'commit', '-qm', 'base')
            base = git(repo, 'rev-parse', 'HEAD').strip()
            (repo / '[ab].py').write_text('one = 1\ntwo = 22\n')
            (repo / 'a.py').write_text(''.join(f'v{i} = {i * (i != 31) + 7 * (i == 31)}\n' for i in range(40)))
            git(repo, '-c', 'commit.gpgsign=false', 'commit', '-qam', 'head')
            head = git(repo, 'rev-parse', 'HEAD').strip()
            olds, news = context.hunks(repo, base, head, '[ab].py', '[ab].py')[:2]
        self.assertEqual((olds, news), ({2}, {2}))

    def test_a_submodule_bump_and_a_readme_folder_do_not_stop_the_build(self):
        # A gitlink made with --cacheinfo: no submodule clone, no network. Its commit is not in this repository.
        with tempfile.TemporaryDirectory() as d:
            repo, base, head = make_repo(d, {'README/notes.txt': 'a folder named README\n'})
            git(repo, 'update-index', '--add', '--cacheinfo', f'160000,{head},sub')
            git(repo, 'rm', '-q', '--cached', 'README.md')  # README.md gone; 'README' is a folder
            git(repo, '-c', 'commit.gpgsign=false', 'commit', '-qm', 'bump sub')
            m = context.build(repo, base, git(repo, 'rev-parse', 'HEAD').strip())
        self.assertIn(f'=== sub (after the change) ===\n(submodule at commit {head};', m)
        self.assertIn(f'Changed submodules, shown only as their commit:\n- sub at {head}\n', m)
        self.assertNotIn('=== README', m)
        self.assertIn('callers.py', blocks(m))  # the rest of the change still gets its context

    def test_revisions_must_be_commits_and_cannot_become_options(self):
        for base in ('--output=/tmp/x', '', 'nope', '0' * 40):
            with self.subTest(base=base), self.assertRaises(context.ContextError):
                context.build(self.repo, base, self.head)
        with self.assertRaises(context.ContextError):
            context.build(Path(self.tmp.name) / 'missing', self.base, self.head)

    def test_an_incomplete_partial_clone_is_refused_without_fetching(self):
        git(self.repo, 'config', 'uploadpack.allowFilter', 'true')
        clone = Path(self.tmp.name) / 'partial'
        subprocess.run(['git', 'clone', '-q', '--filter=blob:none', '--no-checkout', self.repo.as_uri(), str(clone)],
                       check=True, capture_output=True, env=ENV)
        before = git(clone, 'count-objects', '-v')
        with self.assertRaisesRegex(context.ContextError, 'incomplete partial clone'):
            context.build(clone, self.base, self.head)
        self.assertEqual(git(clone, 'count-objects', '-v'), before)  # nothing was fetched lazily

    def test_symbols_from_python_ast_and_from_other_languages(self):
        src = 'class Box:\n    SIZE = 2\n\n    def __init__(self):\n        self.n = 1\n\n    def open(self):\n' \
              '        def helper():\n            return 1\n        return helper()\n'
        self.assertEqual(context.python_symbols(src, {5}), ({'Box'}, {'Box'}))        # dunder stands for its class
        self.assertEqual(context.python_symbols(src, {9}), ({'helper'}, set()))       # innermost def only
        self.assertEqual(context.python_symbols(src, {10}), ({'open'}, {'Box'}))
        self.assertEqual(context.python_symbols(src, {2}), ({'Box', 'SIZE'}, set()))
        self.assertIsNone(context.python_symbols('def (:', {1}))
        found = context.text_symbols(['func (s *Server) Handle(w http.ResponseWriter) {'],
                                     ['pub fn parse_line(s: &str) -> u8 {', 'public int countItems(List x) {',
                                      'return helper(x) {', 'MAX_ITEMS = 10', 'x = 1'])
        self.assertEqual(found, {'Handle', 'parse_line', 'countItems', 'MAX_ITEMS'})
        self.assertEqual(context.kind('class Sub(pkg.Base, Mixin):', 'Base'), 'subclass')
        self.assertEqual(context.kind('public class Sub extends Base implements Api {', 'Api'), 'subclass')
        self.assertEqual(context.kind('impl Display for Base {', 'Display'), 'subclass')
        self.assertEqual(context.kind('def Base(x):', 'Base'), 'definition')
        self.assertEqual(context.kind('y = Base()', 'Base'), 'reference')
        self.assertTrue(context.is_test('pkg/tests/util.py') and context.is_test('foo_test.go'))
        self.assertFalse(context.is_test('src/contest.py'))


class CliTest(unittest.TestCase):
    def test_review_repo_builds_the_material_into_the_trace_and_the_system_prompt(self):
        with tempfile.TemporaryDirectory() as d:
            repo, base, head = make_repo(d)
            Path(d, 'pr.txt').write_text('Bump scale by one')
            seen = []
            def isolated(argv, stdin, timeout, **kw):
                seen.append(stdin)
                return 'What this change does: x.\n\nNo defects found.'
            trace = Path(d) / 'trace'
            with mock.patch.object(backends, 'run_isolated', side_effect=isolated), \
                    mock.patch('sys.stderr', new_callable=io.StringIO) as err, \
                    mock.patch('sys.stdout', new_callable=io.StringIO):
                code = harness.main(['review', '--repo', str(repo), '--base', base, '--head', head,
                                     '--pr-text', str(Path(d, 'pr.txt')), '--context-budget', '200000',
                                     '--backend', 'finder-cmd', '--trace', str(trace)])
            self.assertEqual(code, 0, err.getvalue())
            material = (trace / 'material.txt').read_text()
            self.assertEqual(material, context.build(repo, base, head, 'Bump scale by one', 200_000))
            self.assertIn('=== callers.py (unchanged, at head) ===', material)
            self.assertEqual(len(seen), 1)
            self.assertIn('=== context manifest ===', seen[0])
            self.assertEqual(json.loads(err.getvalue().strip().splitlines()[-1].split('harness: ', 1)[1])['calls'], 1)

    def test_exactly_one_material_source_and_repo_flags_only_with_repo(self):
        with tempfile.TemporaryDirectory() as d:
            m = Path(d, 'm.txt')
            m.write_text('x')
            for argv in (['review'], ['review', '--input', str(m), '--repo', d, '--base', 'a', '--head', 'b'],
                         ['review', '--repo', d, '--base', 'a'], ['review', '--input', str(m), '--base', 'a'],
                         ['review', '--input', str(m), '--context-budget', '10'],
                         ['review', '--repo', d, '--base', 'a', '--head', 'b', '--context-budget', '0'],
                         ['design', '--repo', d, '--base', 'a', '--head', 'b']):
                with self.subTest(argv=argv), mock.patch('sys.stderr', new_callable=io.StringIO), \
                        self.assertRaises(SystemExit) as exit_:
                    harness.main(argv)
                self.assertEqual(exit_.exception.code, 2)
            with mock.patch('sys.stderr', new_callable=io.StringIO) as err:  # not a repository: cannot start, no call
                with mock.patch.object(backends, 'run_isolated') as run:
                    self.assertEqual(harness.main(['review', '--repo', d, '--base', 'a', '--head', 'b']), 2)
                run.assert_not_called()
            self.assertIn('review cannot start', err.getvalue())


class GrepFallbackTest(unittest.TestCase):
    def test_word_matches_are_the_same_with_and_without_pcre(self):
        with tempfile.TemporaryDirectory() as d:
            repo = Path(d)
            subprocess.run(['git', 'init', '-q', str(repo)], check=True)
            (repo / 'a.py').write_text('foo_bar = 1\nfoo = 2\nx = foo(foo_bar)\nbarfoo()\nbar\n')
            subprocess.run(['git', '-C', str(repo), 'add', '.'], check=True)
            subprocess.run(['git', '-C', str(repo), '-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-qm', 'x'], check=True)
            fast = context.grep(repo, 'HEAD', ['foo', 'bar'])
            real = context.git
            def no_pcre(r, *args, **kw):
                if '-P' in args:
                    raise context.ContextError('git grep failed: fatal: cannot use Perl-compatible regexes')
                return real(r, *args, **kw)
            with mock.patch.object(context, 'git', side_effect=no_pcre):
                plain = context.grep(repo, 'HEAD', ['foo', 'bar'])
            self.assertEqual(fast, plain)
            self.assertEqual([r.split('\0')[1] for r in fast.split('\n') if r], ['2', '3', '5'])
            self.assertEqual(context.grep(repo, 'HEAD', ['café']), '')  # non-ASCII names take the -F -w path

    def test_word_boundaries_beside_unicode_letters_match_git_w_in_a_utf8_locale(self):
        with tempfile.TemporaryDirectory() as d, mock.patch.dict(os.environ, {'LC_ALL': 'en_US.UTF-8'}):
            repo = Path(d)
            subprocess.run(['git', 'init', '-q', str(repo)], check=True)
            (repo / 'a.py').write_text('caf\u00e9foo()\nfoo\u00e9 = 1\nfoo()\nna\u00efve_foo\n\u03a3foo\n', encoding='utf-8')
            subprocess.run(['git', '-C', str(repo), 'add', '.'], check=True)
            subprocess.run(['git', '-C', str(repo), '-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-qm', 'x'], check=True)
            fast = context.grep(repo, 'HEAD', ['foo'])
            plain = context.git(repo, 'grep', '-n', '-I', '-z', '--no-color', '--no-column', '-w', '-F', '-e', 'foo',
                                'HEAD', '--', ok=(0, 1))
            self.assertEqual(fast, plain)  # git -w counts only ASCII letters, digits and _ as word characters

    def test_a_truncated_batch_object_is_refused(self):
        class Cut:
            def __init__(self):
                self.stdin, self.stdout = io.BytesIO(), io.BytesIO(b'0123456789abcdef0123456789abcdef01234567 blob 10\nshort')
        objects = context.Objects(Path('.'))
        objects.proc = Cut()
        with self.assertRaisesRegex(context.ContextError, 'truncated'):
            objects.read('HEAD', 'a.py')
        objects.proc = None


if __name__ == '__main__':
    unittest.main()
