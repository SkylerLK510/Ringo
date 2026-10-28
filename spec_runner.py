"""Run a generated spec-test module against a candidate folder; write each test's outcome as JSON.

usage: python3 -I spec_runner.py <candidate_root> <tests.py> --json OUT

The candidate root goes first on sys.path and becomes the working directory. The module's suite runs as unittest runs
it, so class and module fixtures (setUpClass, setUpModule) apply; each test gets a 10 s alarm. Test ids are
'<Class>.<test>'; a failed class or module fixture fails every test it covers with its error; a module that fails
to load is reported as the single id '<import>'.
"""
import importlib.util
import json
import os
import signal
import sys
import traceback
import unittest
from pathlib import Path

PER_TEST_S = 10


def _alarm(signum, frame):
    raise TimeoutError(f'test exceeded {PER_TEST_S} s')


def tests_in(suite):
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from tests_in(item)
        else:
            yield item


def test_id(test):
    return f'{type(test).__name__}.{test._testMethodName}'


class Record(unittest.TestResult):
    """Each test's outcome by id; the alarm runs only while a test does."""
    def __init__(self):
        super().__init__()
        self.outcomes = {}

    def startTest(self, test):
        super().startTest(test)
        self.before = len(self.failures), len(self.errors)
        signal.alarm(PER_TEST_S)

    def stopTest(self, test):
        signal.alarm(0)
        bad = self.failures[self.before[0]:] + self.errors[self.before[1]:]
        self.outcomes[test_id(test)] = {'ok': not bad, 'error': bad[0][1][-2000:] if bad else ''}
        super().stopTest(test)


def main(argv):
    out = Path(argv[argv.index('--json') + 1])
    root, path = Path(argv[0]).resolve(), Path(argv[1]).resolve()
    sys.path.insert(0, str(root))
    os.chdir(root)
    results = {}
    try:
        spec = importlib.util.spec_from_file_location('spec_tests', path)
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module  # unittest finds setUpModule and tearDownModule through sys.modules
        spec.loader.exec_module(module)
        suite = unittest.defaultTestLoader.loadTestsFromModule(module)
    except BaseException:
        results['<import>'] = {'ok': False, 'error': traceback.format_exc(limit=4)[-2000:]}
        suite = unittest.TestSuite()
    ids = [test_id(t) for t in tests_in(suite)]  # before running: a suite drops its tests as it runs them
    signal.signal(signal.SIGALRM, _alarm)
    record = Record()
    try:
        suite.run(record)
    except BaseException as exc:  # a timeout or SystemExit that escaped unittest
        record.errors.append((None, repr(exc)))
    for holder, error in record.errors:
        if isinstance(holder, unittest.TestCase):
            continue
        # A class or module fixture failed: "setUpClass (spec_tests.Name)" or "tearDownModule (spec_tests)". Its error
        # fails every test it covers, run or not, as a failed fixture fails the suite in unittest.
        desc = getattr(holder, 'description', '')
        name = desc.rsplit('.', 1)[-1].rstrip(')') if '.' in desc else None
        for tid in ids:
            if name is None or tid.split('.')[0] == name:
                if record.outcomes.get(tid, {}).get('ok', True):
                    record.outcomes[tid] = {'ok': False, 'error': f'{desc or "suite"}: {error}'[-2000:]}
    for tid in ids:
        results[tid] = record.outcomes.get(tid) or {'ok': False, 'error': 'not run'}
    passed = sum(r['ok'] for r in results.values())
    out.write_text(json.dumps({'passed': passed, 'total': len(results), 'tests': results}, indent=1))
    print(f'{passed}/{len(results)} spec tests pass')
    return 0 if results and passed == len(results) else 1


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
