"""Ringo CLI: review, design and bounded council-guided work."""
import argparse
import sys

import harness


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv in ([], ['--help'], ['-h']):
        parser = argparse.ArgumentParser(prog='ringo.py', description=__doc__)
        commands = parser.add_subparsers(dest='command')
        for name, help_text in [('review', 'review code'), ('design', 'propose a design'),
                                ('work', 'revise artifacts with checks and council assessment')]:
            commands.add_parser(name, help=help_text)
        parser.print_help()
        return 0 if argv else 2
    if argv[:1] == ['work']:
        import workflow
        return workflow.main(argv[1:])
    return harness.main(argv, prog='ringo.py')


if __name__ == '__main__':
    sys.exit(main())
