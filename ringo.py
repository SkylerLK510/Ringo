"""Ringo CLI: review, design, research and bounded council-guided work."""
import argparse
import sys

# wait's exit codes: ready or done 0, finished without a ready result 1, failed or gone 2, still running 3
WAIT_EXIT = {'done': 0, 'ready_for_review': 0, 'needs_revision': 1, 'repeated_candidate': 1, 'budget_exhausted': 1}


def check_pid(pid):
    """Return if a process with this pid exists; raise ProcessLookupError if none does, PermissionError if it exists
    but may not be asked. POSIX: os.kill(pid, 0). On Windows os.kill(pid, 0) terminates the process, so the process
    is opened for a zero-timeout wait instead: an exited process whose handle someone still holds counts as gone."""
    import os
    if os.name != 'nt':
        os.kill(pid, 0)
        return
    if type(pid) is not int:
        raise TypeError(f'pid must be an int, not {type(pid).__name__}')
    if not 0 <= pid <= 0xFFFFFFFF:
        raise OverflowError('pid out of range')
    import ctypes
    from ctypes import wintypes
    k32 = ctypes.WinDLL('kernel32', use_last_error=True)
    k32.OpenProcess.restype, k32.OpenProcess.argtypes = wintypes.HANDLE, (wintypes.DWORD, wintypes.BOOL, wintypes.DWORD)
    k32.WaitForSingleObject.restype = wintypes.DWORD
    k32.WaitForSingleObject.argtypes = (wintypes.HANDLE, wintypes.DWORD)
    k32.CloseHandle.argtypes = (wintypes.HANDLE,)
    handle = k32.OpenProcess(0x00100000 | 0x1000, False, pid)  # SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION
    if not handle:
        error = ctypes.get_last_error()
        if error == 87:  # ERROR_INVALID_PARAMETER: no process has this pid
            raise ProcessLookupError(pid)
        raise PermissionError(error, ctypes.FormatError(error))
    try:
        if k32.WaitForSingleObject(handle, 0) == 0:  # WAIT_OBJECT_0: it has exited
            raise ProcessLookupError(pid)
    finally:
        k32.CloseHandle(handle)


def wait(argv):
    """Block until a run's state.json leaves 'running', then print it as one line. A deterministic wait: one
    command instead of an agent re-entering its model to sleep and poll. Needs no runtime bundle."""
    import json, os, time
    from pathlib import Path
    parser = argparse.ArgumentParser(prog='ringo.py wait', description=wait.__doc__)
    parser.add_argument('folder', help='the --trace folder of a review/design run, or the --out folder of a work run')
    parser.add_argument('--timeout', type=float, default=540,
                        help='seconds to wait before reporting the run as still running (default 540)')
    args = parser.parse_args(argv)
    path, deadline, state = Path(args.folder) / 'state.json', time.monotonic() + args.timeout, None
    while True:
        try:
            state = json.loads(path.read_text(encoding='utf-8'))
        except (OSError, ValueError):  # not written yet
            state = None
        if state and state.get('status') == 'running':
            try:
                check_pid(state['pid'])
            except ProcessLookupError:  # it may have finished between the read and the check: read again
                try:
                    state = json.loads(path.read_text(encoding='utf-8'))
                except (OSError, ValueError):
                    pass
                if state.get('status') == 'running':
                    state = {**state, 'status': 'interrupted', 'error': 'the run process is gone'}
            except (PermissionError, KeyError, TypeError, OverflowError):
                pass  # another user's process, or no pid: only the timeout ends the wait
        if state and state.get('status') != 'running' or time.monotonic() >= deadline:
            break
        time.sleep(0.1)  # state.json is under 1 KB: a finished run is seen within 0.1 s
    if not state:
        print(json.dumps({'status': 'not_started', 'folder': str(path.parent)}))
        return 3
    print(json.dumps({k: v for k, v in state.items() if k not in ('version', 'pid')}, separators=(',', ':')))
    return 3 if state['status'] == 'running' else WAIT_EXIT.get(state['status'], 2)


def main(argv=None):
    import os
    if os.environ.get('RINGO_CHILD'):  # set by the ringo shell for the agents it launches
        print('ringo: refusing to start inside an agent that ringo launched (RINGO_CHILD is set)', file=sys.stderr)
        return 2
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv[:1] == ['wait']:
        return wait(argv[1:])
    import releases
    if argv[:1] == ['release']:
        return releases.main(argv[1:])
    try:
        result = releases.dispatch(argv)
    except (ValueError, OSError, KeyError, TypeError) as exc:
        print(f'ringo release: {exc}', file=sys.stderr)
        return 2
    if result is not None:
        return result
    if argv in ([], ['--help'], ['-h']):
        parser = argparse.ArgumentParser(prog='ringo.py', description=__doc__)
        commands = parser.add_subparsers(dest='command')
        for name, help_text in [('review', 'review code'), ('design', 'propose a design'),
                                ('work', 'revise artifacts with checks and one review'),
                                ('research', 'search the web once, verify quotes in code, write one cited report'),
                                ('wait', 'block until a traced run finishes and print its state'),
                                ('release', 'stage, activate, inspect or roll back local releases')]:
            commands.add_parser(name, help=help_text)
        parser.print_help()
        return 0 if argv else 2
    if argv[:1] == ['work']:
        import workflow
        return workflow.main(argv[1:])
    if argv[:1] == ['research']:
        import research
        return research.main(argv[1:])
    import harness
    return harness.main(argv, prog='ringo.py')


if __name__ == '__main__':
    sys.exit(main())
