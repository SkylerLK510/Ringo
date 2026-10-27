"""Run a generated spec-test module against a candidate folder; write each test's outcome as JSON.

usage: python3 -I spec_runner.py <candidate_root> <tests.py> --json OUT

The candidate root goes first on sys.path and becomes the working directory. Each test gets a 10 s alarm. Test ids
are '<Class>.<test>'; a module that fails to load is reported as the single id '<import>'.
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


def main(argv):
    out = Path(argv[argv.index('--json') + 1])
    root, path = Path(argv[0]).resolve(), Path(argv[1]).resolve()
    sys.path.insert(0, str(root))
    os.chdir(root)
    results = {}
    try:
        spec = importlib.util.spec_from_file_location('spec_tests', path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        suite = unittest.defaultTestLoader.loadTestsFromModule(module)
    except BaseException:
        results['<import>'] = {'ok': False, 'error': traceback.format_exc(limit=4)[-2000:]}
        suite = []
    signal.signal(signal.SIGALRM, _alarm)
    for test in tests_in(suite):
        tid = f'{type(test).__name__}.{test._testMethodName}'
        result = unittest.TestResult()
        signal.alarm(PER_TEST_S)
        try:
            test.run(result)
        except BaseException as exc:  # a timeout or SystemExit that escaped the test
            result.errors.append((test, repr(exc)))
        finally:
            signal.alarm(0)
        bad = result.failures + result.errors
        results[tid] = {'ok': not bad, 'error': bad[0][1][-2000:] if bad else ''}
    passed = sum(r['ok'] for r in results.values())
    out.write_text(json.dumps({'passed': passed, 'total': len(results), 'tests': results}, indent=1))
    print(f'{passed}/{len(results)} spec tests pass')
    return 0 if results and passed == len(results) else 1


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
