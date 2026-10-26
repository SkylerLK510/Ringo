"""The improvement log admits only what its committed evidence shows, and the page is never hand-edited."""
import copy
import hashlib
import json
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import improvement_log as log


class ImprovementLogTest(unittest.TestCase):
    def test_page_is_exactly_what_the_evidence_produces(self):
        self.assertEqual(log.PAGE.read_text(encoding='utf-8'), log.render(),
                         'run `python3 improvement_log.py --write` and commit the result')

    def tampered(self, change, evidence=None):
        """check() of one edited entry against a private copy of the evidence; evidence(name, data) may edit a file."""
        d = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, d)
        shutil.copytree(log.LOG.parent, d / 'improvements')
        entries = json.loads(log.LOG.read_text())
        entry = change(copy.deepcopy(entries))
        if evidence:
            for name in entry['evidence']:
                path = d / 'improvements' / name
                data = json.loads(path.read_text())
                if evidence(name, data):
                    path.write_text(json.dumps(data))
                    entry['evidence'][name] = hashlib.sha256(path.read_bytes()).hexdigest()
        with mock.patch.object(log, 'LOG', d / 'improvements' / 'log.json'):
            return log.check(entry)

    @staticmethod
    def kind(entries, kind, title=None):
        return next(e for e in entries if e['kind'] == kind and (title is None or e['title'] == title))

    def test_every_entry_checks(self):
        for entry in json.loads(log.LOG.read_text()):
            with self.subTest(entry=entry['title']):
                self.assertTrue(log.check(entry))

    def test_claims_the_evidence_does_not_show_are_refused(self):
        launch = lambda e: self.kind(e, 'latency', 'Shorter launch for one-call plans')
        cases = {
            'interval includes no change': lambda e: {**launch(e), 'claim': ['review', 'startup']},
            'failed arm': lambda e: {**launch(e), 'claim': ['review-failure']},
            'unmeasured workload': lambda e: {**launch(e), 'claim': ['review', 'nowhere']},
            'number typed into prose': lambda e: {**launch(e), 'title': 'Launch is 3 ms faster'},
            'unknown field': lambda e: {**launch(e), 'note': 'x'},
            'tokens wrong date': lambda e: {**self.kind(e, 'tokens'), 'date': '2026-10-27'},
            'tokens unclaimed pair': lambda e: {**self.kind(e, 'tokens'), 'claim': ['writer']},
        }
        for name, change in cases.items():
            with self.subTest(name), self.assertRaises(ValueError):
                self.tampered(change)

    def test_edited_or_reversed_evidence_is_refused(self):
        launch = lambda e: self.kind(e, 'latency', 'Shorter launch for one-call plans')
        with self.subTest('edited after logging'), self.assertRaisesRegex(ValueError, 'evidence changed'):
            d = Path(tempfile.mkdtemp())
            self.addCleanup(shutil.rmtree, d)
            shutil.copytree(log.LOG.parent, d / 'improvements')
            entry = launch(json.loads(log.LOG.read_text()))
            name = next(iter(entry['evidence']))
            with open(d / 'improvements' / name, 'a') as f:
                f.write(' ')
            with mock.patch.object(log, 'LOG', d / 'improvements' / 'log.json'):
                log.check(entry)

        def swap(_, data):  # the same raw pairs with the arms exchanged: a demonstrated slowdown
            for report in data['reports']:
                for row in report['measurement']['pairs']:
                    row['baseline_s'], row['candidate_s'] = row['candidate_s'], row['baseline_s']
            return True
        with self.subTest('slowdown in the same evidence'), self.assertRaisesRegex(ValueError, 'slowdown'):
            self.tampered(launch, swap)

        def reverse(_, data):
            data['pairs'] = [[b, a] for a, b in data['pairs']]
            return True
        with self.subTest('more tokens after'), self.assertRaisesRegex(ValueError, 'fewer input tokens'):
            self.tampered(lambda e: self.kind(e, 'tokens'), reverse)

        def duplicate(_, data):  # a failing copy of review hidden behind a clean one
            failing = copy.deepcopy(next(r for r in data['reports'] if r['measurement']['protocol']['workload'] == 'review'))
            failing['measurement']['pairs'][0]['candidate_ok'] = False
            data['reports'].insert(0, failing)
            return True
        with self.subTest('duplicate workload'), self.assertRaisesRegex(ValueError, 'more than one report'):
            self.tampered(launch, duplicate)

        def unpinned(_, data):
            for c in data['calls']:
                k = c['argv'].index('--model')
                del c['argv'][k:k + 2]
            return True
        with self.subTest('model not pinned'), self.assertRaisesRegex(ValueError, 'must pin'):
            self.tampered(lambda e: self.kind(e, 'tokens'), unpinned)

        def other_input(_, data):
            next(c for c in data['calls'] if c['id'] == 'lean-judge')['stdin_sha256'] = '0' * 64
            return True
        with self.subTest('different input'), self.assertRaisesRegex(ValueError, 'differ in input'):
            self.tampered(lambda e: self.kind(e, 'tokens'), other_input)


if __name__ == '__main__':
    unittest.main()
