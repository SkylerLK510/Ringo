"""Offline regressions for trace preservation, review parsing and campaign gates."""
import hashlib
import json
import shutil
import subprocess
import tempfile
import unittest
import zlib
from pathlib import Path
from unittest import mock

import evaluate as ev
import harness as h
import improve as im
import pipelines as pl
from tests.test_harness import B, REVIEW, USAGE, plan
from tests.test_improve import BASE_PROMPT, World, edit
from tests.test_evaluate import Store, fake_call, fake_identity


class ReviewRuntimeTest(unittest.TestCase):
    def test_generated_batch_and_repair_labels_preserve_every_reply(self):
        for suffix, batch in [('b1', 1), ('repair', None)]:
            with self.subTest(suffix=suffix), tempfile.TemporaryDirectory() as d:
                reader = f'k-{suffix}'
                passes = [{'id': reader, 'prompt': 'r.md', 'reply': 'findings'},
                          {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}]
                if batch:
                    passes[-1]['batch'] = batch
                p = plan(passes, d=d)
                count = 0
                def call(b, s, message, timeout, model=None):
                    nonlocal count
                    count += 1
                    if 'INSTRUCTION k-' in message:
                        return REVIEW, USAGE
                    if not batch and count == 2:
                        return f'{reader}.1 KEEP', USAGE
                    return f'{reader}.1 KEEP\n{reader}.2 KEEP', USAGE
                trace = Path(d) / 'trace'
                _, stats = h.run_plan(p, 'M', B, h.Run(trace), call_fn=call)
                replies = [f.read_text() for f in (trace / 'steps').glob('*.txt')]
                self.assertEqual(len(replies), count)
                self.assertIn(REVIEW, replies)
                self.assertEqual(stats['kept'], 2)

    def test_findings_after_terminal_sections_are_retried_instead_of_lost(self):
        p = plan([{'id': 'r', 'prompt': 'r.md', 'reply': 'findings'},
                  {'id': 'k', 'prompt': 'k.md', 'reply': 'verdicts'}])
        for section in ['### Open questions', '### Not checked']:
            with self.subTest(section=section), self.assertRaisesRegex(ValueError, 'after'):
                pl.check_reply(p, p['passes'][0], section + '\n- none\n### 1. Real bug\nEvidence.')

    def test_nested_section_words_do_not_truncate_findings(self):
        for heading in ['#### Not checked for null', '#### Open questions about ownership', '#### Not checked']:
            text = '### 1. Null dereference\nEvidence.\n' + heading + '\nExplanation.\n### 2. Leak\nMore evidence.'
            with self.subTest(heading=heading):
                _, found, sections = pl.findings('r', text)
                self.assertEqual(len(found), 2)
                self.assertIn('Explanation.', found['r.1'])
                self.assertEqual(sections, {})

    def test_unclosed_inner_fences_hide_quoted_headings_through_eof(self):
        for fence in ['```markdown', '~~~~']:
            text = f'### 1. Real bug\nExample:\n{fence}\n### 2. Sample data\n### Not checked\nQuoted content'
            with self.subTest(fence=fence):
                _, found, sections = pl.findings('r', text)
                self.assertEqual(list(found), ['r.1'])
                self.assertIn('Quoted content', found['r.1'])
                self.assertEqual(sections, {})


class CampaignRuntimeTest(unittest.TestCase):
    def world(self, proposals=()):
        w = World(proposals)
        self.addCleanup(shutil.rmtree, w.dir)
        return w

    def test_hidden_contamination_precedes_dev_and_structural_rejections(self):
        w = self.world()
        camp = im.load_campaign(w.campaign)
        dev = ev.split_cases(w.store, 'dev')
        hidden = ev.split_cases(w.store, 'validation')[0]
        canary = json.loads((w.store / 'canaries.json').read_text())[hidden]
        proposals = [edit(BASE_PROMPT + ' ' + dev[0] + ' ' + hidden),
                     {'hypothesis': hidden, 'plan': {'kind': 'design'}},
                     {'hypothesis': canary, 'prompts': {'../escape.md': 'x'}},
                     {'hypothesis': 'h', 'prompts': {canary + '.md': 'x'}}]
        for proposal in proposals:
            with self.subTest(proposal=proposal), self.assertRaisesRegex(ev.EvalError, 'contamination'):
                im.screen(w.store, proposal, w.plan, dev, camp)

    def test_campaign_rejects_changed_dev_artifacts_before_validation(self):
        w = self.world([edit(BASE_PROMPT + ' For each defect, state the failing input first.')])
        w.campaign.write_text(json.dumps({**json.loads(w.campaign.read_text()), 'max_proposals': 1}))
        real_grade = ev.grade
        def grade(store, d, **kwargs):
            real_grade(store, d, **kwargs)
            spec = ev.prereg(store, d)['spec']
            if spec['split'] == 'dev' and spec['comparators']:
                result = next((Path(store) / 'runs' / d / 'out').glob('*/cand.*/result.md'))
                result.write_text(result.read_text() + '\nChanged after grading.\n')
        with mock.patch.object(ev, 'identity', fake_identity), mock.patch.object(ev, 'grade', side_effect=grade):
            log = im.campaign(w.store, w.campaign, w.dir / 'ws', call_fn=w.call)
        self.assertFalse(any(r['kind'] == 'prereg' and r['split'] == 'validation' for r in ev.ledger(w.store)))
        self.assertEqual(log[0]['result'], 'rejected')

    def test_malformed_proposer_reply_cannot_hide_a_canary(self):
        w = self.world()
        calls = []
        def call(backend, *args):
            if backend['argv'][0] == 'proposer':
                calls.append(1)
                return 'not JSON: canary0000000000000000000000000001', USAGE
            return w.call(backend, *args)
        with mock.patch.object(ev, 'identity', fake_identity), self.assertRaisesRegex(ev.EvalError, 'contamination'):
            im.campaign(w.store, w.campaign, w.dir / 'ws', call_fn=call)
        self.assertEqual(calls, [1])


class EvaluationProvenanceTest(unittest.TestCase):
    def setUp(self):
        patch = mock.patch.object(ev, 'identity', fake_identity)
        patch.start()
        self.addCleanup(patch.stop)

    def test_canary_map_changes_invalidate_registration(self):
        for remove in (False, True):
            s = Store(groups=1, fractions=(1, 0, 0))
            d = ev.freeze(s.dir, s.spec(split='dev'))
            canaries = s.dir / 'canaries.json'
            if remove:
                canaries.unlink()
            else:
                canaries.write_text('{}')
            with self.subTest(remove=remove), self.assertRaisesRegex(ev.EvalError, 'input changed'):
                ev.checked(s.dir, d)

    def test_unsealed_reviews_are_refused_before_any_backend_call(self):
        s = Store(groups=1, fractions=(1, 0, 0))
        d = ev.freeze(s.dir, s.spec(split='dev', runs=1))
        for cid, arm, r in ev.slots(s.dir, ev.prereg(s.dir, d)['spec']):
            where = s.dir / 'runs' / d / 'out' / cid / f'{arm}.{r}'
            where.mkdir(parents=True)
            (where / 'result.md').write_text(REVIEW)
            (where / 'summary.json').write_text(json.dumps({'calls': 0, 'cost_usd': 0, 'unpriced_calls': 0}))
        calls = []
        with self.assertRaisesRegex(ev.EvalError, 'unsealed'):
            ev.run(s.dir, d, call_fn=lambda *args: calls.append(args))
        self.assertEqual(calls, [])

    def test_unsealed_blind_keys_and_grades_cannot_produce_a_pass(self):
        for sealed_key in (False, True):
            with self.subTest(sealed_key=sealed_key):
                s = Store(groups=10, fractions=(1, 0, 0))
                d = ev.freeze(s.dir, s.spec(split='dev', runs=1))
                ev.run(s.dir, d, call_fn=fake_call)
                root = s.dir / 'runs' / d
                key = {f'r-{n}': f'{c}/{a}.{r}' for n, (c, a, r) in enumerate(ev.slots(s.dir, ev.prereg(s.dir, d)['spec']))}
                (s.dir / 'keys').mkdir()
                keyfile = s.dir / 'keys' / f'{d}.json'
                keyfile.write_text(json.dumps(key, indent=1))
                if sealed_key:
                    ev.record(s.dir, 'blind_key', digest=d, artifact=ev.file_sha(keyfile))
                for grader in ev.prereg(s.dir, d)['spec']['graders']:
                    gdir = root / 'grades' / ev.sha(grader)[:12]
                    gdir.mkdir(parents=True)
                    for cid in ev.split_cases(s.dir, 'dev'):
                        grades = {rid: {'found': ['G1', 'G2'] if '/cand.' in slot else ['G1'], 'invalid': []}
                                  for rid, slot in key.items() if slot.startswith(cid + '/')}
                        (gdir / f'{cid}.json').write_text(json.dumps(grades))
                self.assertEqual(ev.decide(s.dir, d)[0], 'inconclusive')
                with self.assertRaisesRegex(ev.EvalError, 'unsealed'):
                    ev.grade(s.dir, d, call_fn=fake_call)


@unittest.skipUnless(shutil.which('git'), 'Git is required for importer fixtures')
class ImportRuntimeTest(unittest.TestCase):
    def import_case(self, files):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        repo = root / 'clones' / 'owner_repo'
        repo.mkdir(parents=True)
        subprocess.run(['git', 'init', '-q', str(repo)], check=True, capture_output=True)

        def obj(kind, data):
            payload = kind.encode() + b' ' + str(len(data)).encode() + b'\0' + data
            digest = hashlib.sha1(payload).hexdigest()
            path = repo / '.git' / 'objects' / digest[:2] / digest[2:]
            path.parent.mkdir(exist_ok=True)
            path.write_bytes(zlib.compress(payload))
            return digest

        def commit(contents, parent=None):
            tree = obj('tree', b''.join(b'100644 ' + name.encode() + b'\0' + bytes.fromhex(obj('blob', value))
                                       for name, value in sorted(contents.items())))
            # Synthetic fixture objects only; no user repository or refs are changed.
            body = f'tree {tree}\n' + (f'parent {parent}\n' if parent else '')
            body += 'author Test <test@example.invalid> 1700000000 +0000\ncommitter Test <test@example.invalid> 1700000000 +0000\n\nfixture\n'
            return obj('commit', body.encode())

        base = commit({'README.md': b'Fixture repository\n'})
        head = commit({'README.md': b'Fixture repository\n', **files}, base)
        corpus = root / 'corpus'
        (corpus / 'corpus').mkdir(parents=True)
        (corpus / 'golden').mkdir()
        manifest = {'nwo': 'owner/repo', 'pr_number': 1, 'base': base, 'head': head,
                    'title': 'Fixture change', 'body': '', 'language': 'Python'}
        (corpus / 'corpus' / 'manifest.json').write_text(json.dumps([manifest]))
        finding = {'tp_fp': 'tp', 'scope': 'introduced-by-pr', 'severity': 'high', 'category': 'correctness',
                   'difficulty': 'easy', 'file': next(iter(files)), 'start_line': 1, 'end_line': 1, 'message': 'Fixture defect'}
        (corpus / 'golden' / f'owner_repo_1-{head[:8]}.json').write_text(json.dumps({'findings': [finding]}))
        store = root / 'store'
        return store, ev.import_reviewbench(store, corpus, root / 'clones')

    def test_changed_filenames_are_read_without_git_quoting(self):
        names = ['café.py', 'tab\tname.py', 'line\nname.py', 'carriage\rreturn.py']
        store, pool = self.import_case({n: f'# {i}\n'.encode() for i, n in enumerate(names)})
        self.assertEqual(list(pool), ['owner_repo_1'])
        material = (store / 'cases' / 'owner_repo_1' / 'material.txt').read_bytes().decode('utf-8')
        for i, name in enumerate(names):
            self.assertIn(f'=== {name} (after the change) ===\n# {i}\n', material)

    def test_nontext_cases_are_skipped_whole_and_recorded(self):
        for blob in [b'\xff\0binary', b'\0valid-UTF8-binary']:
            with self.subTest(blob=blob):
                store, pool = self.import_case({'image.bin': blob, 'code.py': b'# code\n'})
                self.assertEqual(pool, {})
                self.assertFalse((store / 'cases' / 'owner_repo_1').exists())
                self.assertEqual(ev.ledger(store)[-1]['skipped'], [{'case': 'owner_repo_1', 'reason': 'non-text material'}])

    def test_unrepresentable_gold_locations_do_not_corrupt_answer_keys(self):
        for name in ['line\nname.py', 'carriage\rreturn.py', 'tick`name.py']:
            with self.subTest(name=name):
                store, pool = self.import_case({name: b'# code\n'})
                self.assertEqual(pool, {})
                self.assertFalse((store / 'cases' / 'owner_repo_1').exists())
                self.assertEqual(ev.ledger(store)[-1]['skipped'],
                                 [{'case': 'owner_repo_1', 'reason': 'unsupported gold location'}])


if __name__ == '__main__':
    unittest.main()
