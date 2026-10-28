"""Ringo CLI: review, design and bounded council-guided work."""
import argparse
import sys

# wait's exit codes: ready or done 0, finished without a ready result 1, failed or gone 2, still running 3
WAIT_EXIT = {'done': 0, 'ready_for_review': 0, 'needs_revision': 1, 'repeated_candidate': 1, 'budget_exhausted': 1}


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
        # On Windows os.kill(pid, 0) would terminate the process, so only the state file or the timeout ends the wait.
        if state and state.get('status') == 'running' and os.name != 'nt':
            try:
                os.kill(state['pid'], 0)
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
        time.sleep(1)
    if not state:
        print(json.dumps({'status': 'not_started', 'folder': str(path.parent)}))
        return 3
    print(json.dumps({k: v for k, v in state.items() if k not in ('version', 'pid')}, separators=(',', ':')))
    return 3 if state['status'] == 'running' else WAIT_EXIT.get(state['status'], 2)


def main(argv=None):
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
                                ('wait', 'block until a traced run finishes and print its state'),
                                ('release', 'stage, activate, inspect or roll back local releases')]:
            commands.add_parser(name, help=help_text)
        parser.print_help()
        return 0 if argv else 2
    if argv[:1] == ['work']:
        import workflow
        return workflow.main(argv[1:])
    import harness
    return harness.main(argv, prog='ringo.py')


if __name__ == '__main__':
    sys.exit(main())
