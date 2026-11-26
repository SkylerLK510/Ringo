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

# core.autocrlf=input: the files this test writes in text mode (CRLF on Windows) are committed with the LF its
# texts hold, so the repository's objects are the same on every platform.
ENV = {**os.environ, 'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_AUTHOR_NAME': 't',
       'GIT_AUTHOR_EMAIL': 't@t', 'GIT_COMMITTER_NAME': 't', 'GIT_COMMITTER_EMAIL': 't@t',
       'GIT_CONFIG_COUNT': '1', 'GIT_CONFIG_KEY_0': 'core.autocrlf', 'GIT_CONFIG_VALUE_0': 'input'}

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

    def test_a_submodule_bump_to_a_commit_this_repository_lacks_does_not_stop_the_build(self):
        # Newer git answers '<id> submodule' to cat-file --batch for such a gitlink; it used to fail the build.
        with tempfile.TemporaryDirectory() as d:
            repo, base, _ = make_repo(d, {})
            git(repo, 'update-index', '--add', '--cacheinfo', f'160000,{"1" * 40},sub')
            git(repo, '-c', 'commit.gpgsign=false', 'commit', '-qm', 'bump sub')
            m = context.build(repo, base, git(repo, 'rev-parse', 'HEAD').strip())
        self.assertIn(f'Changed submodules, shown only as their commit:\n- sub at {"1" * 40}\n', m)

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


def funcs(n, tag):
    """A Python file of n three-line functions: f<i> is at lines 3i+1 (def), 3i+2 (return) and 3i+3 (blank)."""
    return ''.join(f'def f{i}(x):\n    return x + {i}  # {tag}\n\n' for i in range(n))


def big_repo(d, edits):
    """A repository whose base has two 900-function files (about 38,000 bytes each: over WHOLE_FILE), a small file,
    a lock file, a file marked @generated, a minified-name file and an unchanged user of f100; edits(repo) changes
    some of them for the head commit. Returns (repo, base, head)."""
    repo = Path(d) / 'repo'
    repo.mkdir()
    git(repo, 'init', '-q')
    files = {'a.py': funcs(900, 'a'), 'b.py': funcs(900, 'b'), 'small.py': 'def tiny(x):\n    return x\n',
             'Cargo.lock': 'v1\n' * 20, 'gen.py': '# @generated by tool\nX = 1\n', 'app.min.js': 'var a=1;\n',
             'link.txt': 'plain\n', 'conv.py': b'\x00\x01\x02\n', 'yarn.lock': 'y\n' * 20, 'user.py': 'from a import f100\n\nV = f100(1)\n' + 'z' * 3000 + '\n', 'README.md': '# Big\n'}
    for path, text in files.items():
        (repo / path).parent.mkdir(parents=True, exist_ok=True)
        (repo / path).write_bytes(text if isinstance(text, bytes) else text.encode())
    git(repo, 'add', '-A')
    git(repo, '-c', 'commit.gpgsign=false', 'commit', '-qm', 'base')
    base = git(repo, 'rev-parse', 'HEAD').strip()
    edits(repo)
    git(repo, 'add', '-A')
    git(repo, '-c', 'commit.gpgsign=false', 'commit', '-qm', 'head')
    return repo, base, git(repo, 'rev-parse', 'HEAD').strip()


def edit(repo, path, *pairs):
    text = (repo / path).read_text()
    for old, new in pairs:
        assert old in text, old
        text = text.replace(old, new)
    (repo / path).write_text(text)


def change_alone(repo, base, head):
    """Bytes of the change alone (PR text, diff, changed files, README) in full mode."""
    return len(context.build(repo, base, head, mode='full', budget=1).split('\n=== context manifest ===')[0].encode())


def section(material, label):
    """The text of one '=== label ===' block, up to the next block."""
    return material.split(f'=== {label} ===\n')[1].split('\n=== ')[0]


class WindowedTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def repo(self, edits):
        return big_repo(self.tmp.name, edits)

    def test_windows_are_plus_minus_the_window_around_each_change_and_merge(self):
        # f100's return is line 302, f110's is 332, f500's is 1502: 277-327 and 307-357 touch, 1477-1527 stands alone
        repo, base, head = self.repo(lambda r: edit(r, 'a.py', ('return x + 100 ', 'return x + 1000 '),
                                                    ('return x + 110 ', 'return x + 1100 '), ('return x + 500 ', 'return x + 5000 ')))
        m = context.build(repo, base, head, mode='windowed')
        block = section(m, 'a.py (after the change)')
        self.assertTrue(block.startswith('(windowed: 2700 lines at head; shown: \u00b125 lines around each change and the '
                                         'outline below; other lines are unchanged since the merge base)\n--- lines 277-357 ---\n277: def f92(x):\n'))
        self.assertEqual(block.count('--- lines '), 2)
        self.assertIn('--- lines 1477-1527 ---\n1477: ', block)
        self.assertIn('302:     return x + 1000  # a\n', block)
        self.assertNotIn('276: ', block.split('--- outline ---')[0])
        self.assertIn('- a.py: lines 277-357, 1477-1527 of 2700\n', m)
        narrow = section(context.build(repo, base, head, mode='windowed', changed_window=0), 'a.py (after the change)')
        self.assertIn('--- lines 302-302 ---\n302:     return x + 1000  # a\n--- lines 332-332 ---\n', narrow)

    def test_the_outline_lists_definitions_outside_the_windows_only(self):
        repo, base, head = self.repo(lambda r: edit(r, 'a.py', ('return x + 100 ', 'return x + 1000 ')))
        block = section(context.build(repo, base, head, mode='windowed'), 'a.py (after the change)')
        outline = block.split('--- outline ---\n')[1]
        self.assertTrue(outline.startswith('1: def f0(x):\n4: def f1(x):\n'))
        self.assertIn('274: def f91(x):\n', outline)
        self.assertNotIn('277: def f92(x)', outline)  # inside the window
        self.assertIn('328: def f109(x):\n', outline)
        self.assertRegex(outline, r'\.\.\. and \d+ more\n$')  # 200 lines are listed, the rest summarized
        self.assertEqual(len(outline.splitlines()), 201)

    def test_a_pure_deletion_gets_a_window_where_it_sat(self):
        repo, base, head = self.repo(lambda r: edit(r, 'a.py', ('def f300(x):\n    return x + 300  # a\n\n', '')))
        m = context.build(repo, base, head, mode='windowed')
        block = section(m, 'a.py (after the change)')
        self.assertIn('(windowed: 2697 lines at head;', block)
        self.assertIn('--- lines 876-926 ---\n876: ', block)  # the deletion sat at line 901
        self.assertIn('901: def f301(x):\n', block)

    def test_a_small_changed_file_stays_whole_and_so_does_everything_in_full_mode(self):
        repo, base, head = self.repo(lambda r: (edit(r, 'small.py', ('return x', 'return x + 1')),
                                                edit(r, 'a.py', ('return x + 100 ', 'return x + 1000 '))))
        m = context.build(repo, base, head, mode='windowed')
        self.assertIn('=== small.py (after the change) ===\ndef tiny(x):\n    return x + 1\n', m)
        full = context.build(repo, base, head, mode='full')
        self.assertIn('=== a.py (after the change) ===\ndef f0(x):\n    return x + 0  # a\n', full)
        self.assertNotIn('(windowed:', full)
        self.assertNotIn('changed files: windowed', full)
        self.assertNotIn('Changed files sent as windows', full)
        self.assertLess(len(m), len(full) // 2)

    def test_lock_and_generated_files_become_a_stat_line_in_the_file_block_and_the_diff(self):
        repo, base, head = self.repo(lambda r: ((r / 'Cargo.lock').write_text('v1\nv1\nv2\nv3\nv4\n' + 'v1\n' * 17),
                                                edit(r, 'gen.py', ('X = 1', 'X = 2')), edit(r, 'app.min.js', ('a=1', 'a=2')),
                                                edit(r, 'small.py', ('return x', 'return x + 1'))))
        m = context.build(repo, base, head, mode='windowed')
        self.assertIn('=== Cargo.lock (generated or lock file; not shown) ===\n+3 -1 lines\n', m)
        self.assertIn('=== gen.py (generated or lock file; not shown) ===\n+1 -1 lines\n', m)
        self.assertIn('=== app.min.js (generated or lock file; not shown) ===\n+1 -1 lines\n', m)
        diff = section(m, 'change.diff')
        self.assertIn('diff --git a/Cargo.lock b/Cargo.lock\n(generated or lock file: +3 -1 lines; not shown)\ndiff --git a/app.min.js', diff)
        self.assertNotIn('v4', m)
        self.assertNotIn('X = 2', section(m, 'gen.py (generated or lock file; not shown)'))
        self.assertIn('+X = 2', diff)  # marked only by its own text: the file block is a stat line, the diff stays
        self.assertIn('diff --git a/small.py b/small.py\nindex ', diff)  # other sections stay
        manifest = m.split('=== context manifest ===')[1]
        self.assertIn('(file block and diff):\n- Cargo.lock: +3 -1 lines\n- app.min.js: +1 -1 lines\nGenerated or lock files whose diff was kept:\n'
                      '- gen.py: +1 -1 lines; file block shown as a stat line (marked generated); diff kept\n', manifest)
        self.assertIn('v4', context.build(repo, base, head, mode='full'))

    def test_a_line_over_1000_characters_is_cut_in_windows_and_context_but_not_in_the_diff(self):
        long = 'Q' * 2500
        repo, base, head = self.repo(lambda r: edit(r, 'a.py', ('return x + 100 ', f'return x + 1000 # {long} ')))
        m = context.build(repo, base, head, mode='windowed')
        block = section(m, 'a.py (after the change)')
        row = f'    return x + 1000 # {long}  # a'
        cut = '302: ' + row[:1000] + f' \u2026[{len(row) - 1000} more characters cut]\n'
        self.assertIn(cut, block)
        self.assertNotIn('Q' * 1001, block)
        self.assertIn(long, section(m, 'change.diff'))  # the diff is the change: never cut
        ctx = section(m, 'user.py (unchanged, at head)')
        self.assertIn('z' * 1000 + ' \u2026[2000 more characters cut]', ctx)
        self.assertNotIn('z' * 1001, ctx)
        self.assertIn('z' * 3000, context.build(repo, base, head, mode='full'))

    def test_a_marker_the_change_adds_does_not_classify_the_file(self):
        repo, base, head = self.repo(lambda r: edit(r, 'small.py', ('def tiny(x):\n    return x\n',
                                                                  '# DO NOT EDIT\ndef tiny(x):\n    return x + 1\n')))
        m = context.build(repo, base, head, mode='windowed')
        self.assertIn('=== small.py (after the change) ===\n# DO NOT EDIT\ndef tiny(x):', m)
        self.assertIn('+    return x + 1\n', section(m, 'change.diff'))
        self.assertNotIn('small.py: +', m.split('=== context manifest ===')[1])

    def test_windowing_never_sends_more_than_full(self):
        repo, base, head = self.repo(lambda r: (r / 'added.py').write_text(funcs(900, 'n')))
        windowed = context.build(repo, base, head, mode='windowed')
        # every line of a new file changes, so its windows would be the whole file plus line numbers: it goes whole
        self.assertIn('=== added.py (after the change) ===\ndef f0(x):\n    return x + 0  # n\n', windowed)
        self.assertNotIn('added.py: lines', windowed)
        full = context.build(repo, base, head, mode='full')
        self.assertLessEqual(len(windowed.encode()), len(full.encode()) + 400)  # the manifest's own extra lines
        # auto where neither build fits: the smaller one (here the windowed build is the same size or a little over)
        auto = context.build(repo, base, head, budget=1000)
        sizes = [len(context.build(repo, base, head, mode=m, budget=1000).encode()) for m in ('full', 'windowed')]
        self.assertGreater(min(sizes), 1000)
        self.assertEqual(len(auto.encode()), min(sizes))

    def test_a_type_change_does_not_stop_a_lock_file_from_being_cut_and_odd_names_are_kept(self):
        def edits(r):
            os.remove(r / 'link.txt')
            os.symlink('a.py', r / 'link.txt')
            (r / 'Cargo.lock').write_text('v9\n' + 'v1\n' * 19)
            (r / 'odd dir').mkdir()
            (r / 'odd dir/Cargo.lock').write_text('new\n')
        repo, base, head = self.repo(edits)
        m = context.build(repo, base, head, mode='windowed')
        diff = section(m, 'change.diff')
        self.assertIn('diff --git a/Cargo.lock b/Cargo.lock\n(generated or lock file: +1 -1 lines; not shown)\n', diff)
        self.assertIn('diff --git a/odd dir/Cargo.lock b/odd dir/Cargo.lock\nnew file mode', diff)
        manifest = m.split('=== context manifest ===')[1]
        self.assertIn('- Cargo.lock: +1 -1 lines\n', manifest)
        self.assertIn('- odd dir/Cargo.lock: generated or lock file; diff kept (could not be matched)\n', manifest)

    def test_a_deleted_lock_file_is_cut_from_the_diff(self):
        repo, base, head = self.repo(lambda r: (os.remove(r / 'yarn.lock'), edit(r, 'small.py', ('return x', 'return x + 1'))))
        m = context.build(repo, base, head, mode='windowed')
        self.assertIn('diff --git a/yarn.lock b/yarn.lock\n(generated or lock file: +0 -20 lines; not shown)\n', section(m, 'change.diff'))
        self.assertNotIn('-y\n', m)
        self.assertNotIn('=== yarn.lock', m)
        self.assertIn('- yarn.lock: +0 -20 lines\n', m.split('=== context manifest ===')[1])
        self.assertIn('-y\n', context.build(repo, base, head, mode='full'))

    def test_auto_over_budget_is_never_larger_than_explicit_windowed(self):
        # 100 lines of 166 bytes; changes at lines 1, 44 and 100 leave windows that save fewer bytes than auto's own
        # explanation line costs, so an auto build that compared against its own explained build would return full
        d = Path(self.tmp.name) / 'r'
        d.mkdir()
        git(d, 'init', '-q')
        (d / 'a.py').write_text(''.join(f'L{i:03d} ' + 'x' * 161 + '\n' for i in range(1, 101)))
        git(d, 'add', '-A')
        git(d, '-c', 'commit.gpgsign=false', 'commit', '-qm', 'b')
        base = git(d, 'rev-parse', 'HEAD').strip()
        rows = (d / 'a.py').read_text().split('\n')
        for n in (1, 44, 100):
            rows[n - 1] = rows[n - 1].replace('xxx', 'yyy', 1)
        (d / 'a.py').write_text('\n'.join(rows))
        git(d, '-c', 'commit.gpgsign=false', 'commit', '-qam', 'h')
        head = git(d, 'rev-parse', 'HEAD').strip()
        full = len(context.build(d, base, head, mode='full', budget=1000).encode())
        windowed = context.build(d, base, head, mode='windowed', budget=1000)
        self.assertLess(len(windowed.encode()), full)
        self.assertLess(full - len(windowed.encode()), len('(auto mode: windowed because the change alone is 20000 bytes, over the 1000-byte budget)\n'))
        self.assertEqual(context.build(d, base, head, budget=1000), windowed)

    def test_a_binary_file_that_becomes_marked_text_is_not_classified(self):
        repo, base, head = self.repo(lambda r: (r / 'conv.py').write_text('# DO NOT EDIT\nX = 1\n'))
        m = context.build(repo, base, head, mode='windowed')
        self.assertIn('=== conv.py (after the change) ===\n# DO NOT EDIT\nX = 1\n', m)
        self.assertNotIn('conv.py: +', m)

    def test_a_lock_file_that_becomes_a_symlink_is_cut_to_one_stat_line(self):
        def edits(r):
            os.remove(r / 'Cargo.lock')
            os.symlink('a.py', r / 'Cargo.lock')
        repo, base, head = self.repo(edits)
        diff = section(context.build(repo, base, head, mode='windowed'), 'change.diff')
        self.assertEqual(diff.count('diff --git a/Cargo.lock b/Cargo.lock'), 1)
        self.assertIn('diff --git a/Cargo.lock b/Cargo.lock\n(generated or lock file: +1 -20 lines; not shown)\n', diff)
        manifest = context.build(repo, base, head, mode='windowed').split('=== context manifest ===')[1]
        self.assertIn('- Cargo.lock: +1 -20 lines\n', manifest)
        self.assertNotIn('could not be matched', manifest)

    def test_the_manifest_lists_windowed_and_stat_line_files(self):
        repo, base, head = self.repo(lambda r: (edit(r, 'a.py', ('return x + 100 ', 'return x + 1 ')),
                                                edit(r, 'b.py', ('return x + 3 ', 'return x + 33 ')), (r / 'Cargo.lock').write_text('v9\n' + 'v1\n' * 19)))
        manifest = context.build(repo, base, head, mode='windowed').split('=== context manifest ===')[1]
        self.assertIn('budget 400000 bytes; changed files: windowed.\n', manifest)
        self.assertIn('Changed files sent as windows:\n- a.py: lines 277-327 of 2700\n- b.py: lines 1-36 of 2700\n', manifest)
        self.assertNotIn('auto mode', manifest)

    def test_auto_is_full_under_the_budget_and_windowed_over_it(self):
        repo, base, head = self.repo(lambda r: edit(r, 'a.py', ('return x + 100 ', 'return x + 1000 ')))
        full = context.build(repo, base, head, mode='full')
        self.assertEqual(context.build(repo, base, head), full)
        self.assertEqual(context.build(repo, base, head, mode='auto'), full)
        need = change_alone(repo, base, head)
        self.assertEqual(context.build(repo, base, head, budget=need), context.build(repo, base, head, mode='full', budget=need))
        over = context.build(repo, base, head, budget=need - 1)
        self.assertIn(f'(auto mode: windowed because the change alone is {need} bytes, over the {need - 1}-byte budget)\n', over)
        self.assertIn('(windowed: 2700 lines at head', over)
        self.assertLessEqual(len(over.encode()), need - 1)
        self.assertEqual(context.build(repo, base, head, budget=need - 1), over)  # deterministic

    def test_auto_refill_widens_to_the_whole_file_when_the_overflow_is_small(self):
        repo, base, head = self.repo(lambda r: (edit(r, 'a.py', ('return x + 100 ', 'return x + 1000 ')),
                                                edit(r, 'b.py', ('return x + 100 ', 'return x + 1000 '))))
        need = change_alone(repo, base, head)
        m = context.build(repo, base, head, budget=need - 100)
        self.assertLessEqual(len(m.encode()), need - 100)
        # steps in order: both files at 60, both at 150, then a.py whole; b.py whole would not fit
        self.assertIn('=== a.py (after the change) ===\ndef f0(x):\n    return x + 0  # a\n', m)
        self.assertIn('(windowed: 2700 lines at head; shown: \u00b1150 lines', section(m, 'b.py (after the change)'))
        manifest = m.split('=== context manifest ===')[1]
        self.assertIn('Changed files sent as windows:\n- b.py: lines 152-452 of 2700\n', manifest)
        self.assertNotIn('- a.py: lines', manifest)

    def test_auto_refill_stops_at_the_step_that_still_fits(self):
        repo, base, head = self.repo(lambda r: (edit(r, 'a.py', ('return x + 100 ', 'return x + 1000 ')),
                                                edit(r, 'b.py', ('return x + 100 ', 'return x + 1000 '))))
        size = lambda w: len(context.build(repo, base, head, mode='windowed', changed_window=w).encode())
        budget = (size(60) + size(150)) // 2
        self.assertLess(size(60), budget)
        m = context.build(repo, base, head, budget=budget)
        self.assertLessEqual(len(m.encode()), budget)
        for name in ('a.py', 'b.py'):
            self.assertIn('shown: \u00b160 lines', section(m, f'{name} (after the change)'))
        self.assertNotIn('\u00b1150', m)
        self.assertIn('- a.py: lines 242-362 of 2700\n- b.py: lines 242-362 of 2700\n', m)
        # explicit windowed mode never refills
        self.assertIn('shown: \u00b125 lines', section(context.build(repo, base, head, mode='windowed', budget=budget), 'a.py (after the change)'))

    def test_the_mode_and_the_window_are_validated(self):
        repo, base, head = self.repo(lambda r: edit(r, 'small.py', ('return x', 'return x + 1')))
        for bad in ({'mode': 'wide'}, {'mode': None}, {'changed_window': -1}, {'changed_window': 1.5}):
            with self.subTest(bad=bad), self.assertRaises(context.ContextError):
                context.build(repo, base, head, **bad)

    def test_the_cli_takes_context_mode_only_with_repo(self):
        with tempfile.TemporaryDirectory() as d:
            m = Path(d, 'm.txt')
            m.write_text('x')
            for argv in (['review', '--input', str(m), '--context-mode', 'full'],
                         ['review', '--repo', d, '--base', 'a', '--head', 'b', '--context-mode', 'wide']):
                with self.subTest(argv=argv), mock.patch('sys.stderr', new_callable=io.StringIO) as err, \
                        self.assertRaises(SystemExit) as exit_:
                    harness.main(argv)
                self.assertEqual(exit_.exception.code, 2)
                self.assertRegex(err.getvalue(), r'--context-mode works only with --repo|invalid choice')

    def test_review_repo_passes_the_mode_to_the_material(self):
        repo, base, head = self.repo(lambda r: edit(r, 'a.py', ('return x + 100 ', 'return x + 1000 ')))
        for mode in ('full', 'windowed', None):
            trace = Path(self.tmp.name) / f'trace-{mode}'
            argv = ['review', '--repo', str(repo), '--base', base, '--head', head, '--backend', 'finder-cmd', '--trace', str(trace)]
            with mock.patch.object(backends, 'run_isolated', return_value='No defects found.'), \
                    mock.patch('sys.stderr', new_callable=io.StringIO), mock.patch('sys.stdout', new_callable=io.StringIO):
                self.assertEqual(harness.main(argv + (['--context-mode', mode] if mode else [])), 0)
            self.assertEqual((trace / 'material.txt').read_bytes().decode('utf-8'), context.build(repo, base, head, mode=mode or 'auto'))


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

    def test_a_change_with_thousands_of_names_greps_in_one_short_command(self):
        # git refuses one PCRE alternation past about 31 KB, and the -F -w fallback grows worse than linearly (a merge
        # with 1,899 names ran for minutes): the names go in several patterns of at most GREP_CHUNK bytes, one per
        # stdin line, so the command line stays short (Windows refuses one past 32,767 characters).
        names = [f'name_{i:05d}_symbol' for i in range(2600)]  # about 44 KB of names
        with tempfile.TemporaryDirectory() as d:
            repo = Path(d)
            subprocess.run(['git', 'init', '-q', str(repo)], check=True)
            (repo / 'a.py').write_text('x = name_00000_symbol\ny = name_02599_symbol(name_01300_symbol)\nz = 1\n')
            subprocess.run(['git', '-C', str(repo), 'add', '.'], check=True)
            subprocess.run(['git', '-C', str(repo), '-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-qm', 'x'], check=True)
            pcre = subprocess.run(['git', '-C', str(repo), 'grep', '-P', '-e', 'x', 'HEAD', '--'],
                                  capture_output=True).returncode == 0
            real, seen = context.git, []
            def record(r, *args, **kw):
                seen.append((args, kw.get('input')))
                return real(r, *args, **kw)
            with mock.patch.object(context, 'git', side_effect=record):
                out = context.grep(repo, 'HEAD', names)
            self.assertEqual([r.split('\0')[1] for r in out.split('\n') if r], ['1', '2'], 'each line once, in order')
            self.assertTrue(all(sum(map(len, args)) < 200 for args, _ in seen), 'the patterns are not arguments')
            if pcre:  # git built without PCRE falls back to -F -w, with the same lines
                self.assertEqual(len(seen), 1, 'no -F -w fallback')
                lines = seen[0][1].decode().split('\n')
                self.assertGreater(len(lines), 1)
                self.assertTrue(all(len(line) < 20_000 for line in lines))

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


    def test_noise_paths_are_excluded_from_unchanged_context(self):
        extra = {'package-lock.json': '{"name": "scale"}\n', 'node_modules/pkg/index.js': 'scale(1)\n',
                 'dist/bundle.min.js': 'scale(1)\n'}
        with tempfile.TemporaryDirectory() as d:
            repo, base, head = make_repo(d, extra)
            m = context.build(repo, base, head)
        for noisy in extra:
            self.assertNotIn(noisy, blocks(m))


if __name__ == '__main__':
    unittest.main()
