"""Ringo CLI: review, design and bounded council-guided work."""
import argparse
import sys

def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
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
                                ('work', 'revise artifacts with checks and council assessment'),
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
