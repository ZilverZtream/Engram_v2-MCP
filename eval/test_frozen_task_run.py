"""Protocol tests with synthetic packets, never fresh task-quality evidence."""
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest

import frozen_task_eval as evaluation
import frozen_task_run as collection


class FrozenTaskRunTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='frozen-task-', dir=os.environ.get('ENGRAM_TEST_TMPDIR'))
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.corpus = self.base / 'corpus'
        self.corpus.mkdir()
        cases = []
        frozen = []
        for number, category in enumerate(sorted(evaluation.CATEGORIES)):
            case_id = 'case' + str(number)
            arms = {}
            for arm in ['baseline', 'assisted']:
                name = f'inputs/{case_id}-{arm}.json'
                self.write(self.corpus / name, {'source': 'frozen fact', 'context': arm == 'assisted'})
                frozen.append(name)
                arms[arm] = {'input': name}
            name = f'reads/{case_id}.json'
            self.write(self.corpus / name, {'source': 'additional frozen fact'})
            frozen.append(name)
            cases.append(dict(id=case_id, category=category, task='Explain the supplied fact.', arms=arms,
                              available_reads={f'read{i}': name for i in range(9)}))
        self.oracle = dict(judge_only_canary='JUDGE_ONLY_CANARY',
                           cases={c['id']: {'criteria': {'correct': 'Explain the fact.'}, 'fact': 'frozen fact'} for c in cases})
        self.write(self.corpus / 'oracle.json', self.oracle)
        frozen.append('oracle.json')
        self.manifest = dict(cases=cases, judge_oracle='oracle.json',
                             frozen_files={p: self.sha(self.corpus / p) for p in frozen})
        self.freeze()
        self.root = self.base / 'run'
        self.response = self.base / 'response.json'

    @staticmethod
    def sha(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    @staticmethod
    def write(path, value):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(collection.encoded(value))

    def freeze(self):
        self.write(self.corpus / 'manifest.json', self.manifest)
        self.write(self.corpus / 'freeze_record.json', {'manifest_sha256': self.sha(self.corpus / 'manifest.json')})

    def answer(self, reads=(), decision='The supplied fact is supported.'):
        value = dict(decision=decision, plan=[], evidence=['source'], uncertainties=[], read_requests=list(reads))
        self.write(self.response, value)
        return self.response

    def prepare(self):
        return collection.prepare(self.corpus, self.root, 'protocol_test')

    def finish_candidates(self):
        self.prepare()
        state, _, _ = collection.load_state(self.root)
        for n, run in enumerate(state['runs']):
            actor = 'candidate-' + str(n)
            collection.bind(self.root, run['opaque_id'], actor)
            collection.record(self.root, run['opaque_id'], actor, self.answer())
        return collection.load_state(self.root)[0]

    def judgments(self):
        mapping = evaluation.read_json(self.root, 'judge/mapping.json')
        return [dict(candidate_id=key, criteria={'correct': 'satisfied'}, false_guidance=[], rationale='Synthetic protocol check.') for key in mapping]

    def test_new_run_copies_only_frozen_inputs_and_preserves_original(self):
        self.write(self.corpus / 'judgments.json', {'old': 'must not be copied'})
        before = {str(p.relative_to(self.corpus)): p.read_bytes() for p in self.corpus.rglob('*') if p.is_file()}
        result = self.prepare()
        self.assertEqual(result['completed_candidates'], 0)
        self.assertEqual(len(result['pending_candidates']), 8)
        self.assertFalse((self.root / 'judgments.json').exists())
        for row in result['pending_candidates']:
            self.assertNotIn('arm', row)
        for p in (self.root / 'deliveries-new').iterdir():
            self.assertNotIn(b'JUDGE_ONLY_CANARY', p.read_bytes())
        self.assertEqual(before, {str(p.relative_to(self.corpus)): p.read_bytes() for p in self.corpus.rglob('*') if p.is_file()})
        with self.assertRaises(FileExistsError):
            self.prepare()
        with self.assertRaises(ValueError):
            collection.prepare(self.corpus, self.corpus / 'nested', 'protocol_test')

    def test_full_collection_preserves_response_bytes_and_blinds_judge(self):
        self.prepare()
        state, _, _ = collection.load_state(self.root)
        for n, run in enumerate(state['runs']):
            actor = 'candidate-' + str(n)
            collection.bind(self.root, run['opaque_id'], actor)
            if n == 0:
                collection.record(self.root, run['opaque_id'], actor, self.answer(['read0']))
            self.answer()
            raw = b' \n' + self.response.read_bytes() + b'\n '
            self.response.write_bytes(raw)
            collection.record(self.root, run['opaque_id'], actor, self.response)
        state, manifest, oracle = collection.load_state(self.root)
        self.assertEqual(state['status'], 'awaiting_judge')
        self.assertEqual((self.root / state['runs'][-1]['response']).read_bytes(), raw)
        prompt = evaluation.read_json(self.root, 'judge/prompt.json')
        self.assertEqual(prompt['oracle'], oracle)
        for c in prompt['candidates']:
            self.assertEqual(set(c), {'candidate_id', 'case_id', 'response'})
        self.write(self.response, self.judgments())
        with self.assertRaises(ValueError):
            collection.judge(self.root, 'candidate-0', self.response)
        result = collection.judge(self.root, 'independent-judge', self.response)
        self.assertEqual(result['status'], 'complete')
        metrics = evaluation.results(self.root, manifest, oracle)
        self.assertEqual(sum(m['additional_reads'] for m in metrics), 1)
        self.assertTrue(all(m['output_bytes'] == len(raw) for m in metrics))
        self.assertEqual(evaluation.read_json(self.root, 'report.json')['kind'], 'protocol_test')

    def test_reused_agent_unbound_response_and_wrong_agent_are_rejected(self):
        pending = self.prepare()['pending_candidates']
        a, b = [x['candidate_id'] for x in pending[:2]]
        with self.assertRaises(ValueError):
            collection.record(self.root, a, None, self.answer())
        collection.bind(self.root, a, 'one')
        with self.assertRaises(ValueError):
            collection.bind(self.root, b, 'one')
        with self.assertRaises(ValueError):
            collection.record(self.root, a, 'different', self.answer())
        self.assertEqual(collection.status(self.root)['completed_candidates'], 0)

    def test_read_caps_unknown_reads_and_canary_cannot_advance_state(self):
        candidate = self.prepare()['pending_candidates'][0]['candidate_id']
        collection.bind(self.root, candidate, 'one')
        for reads in [['unknown'], ['read0', 'read0'], ['read0', 'read1', 'read2', 'read3']]:
            with self.assertRaises((ValueError, KeyError)):
                collection.record(self.root, candidate, 'one', self.answer(reads))
        for reads in [['read0', 'read1', 'read2'], ['read3', 'read4', 'read5'], ['read6', 'read7']]:
            collection.record(self.root, candidate, 'one', self.answer(reads))
        before = (self.root / collection.STATE).read_bytes()
        with self.assertRaises(ValueError):
            collection.record(self.root, candidate, 'one', self.answer(['read8']))
        self.answer(decision='JUDGE_ONLY_CANARY')
        self.response.write_bytes(self.response.read_bytes().replace(b'JUDGE', b'\\u004aUDGE'))
        with self.assertRaises(ValueError):
            collection.record(self.root, candidate, 'one', self.response)
        self.assertEqual((self.root / collection.STATE).read_bytes(), before)
        self.assertFalse((self.root / '.collection.lock').exists())

    def test_changed_packet_and_live_lock_are_not_overwritten(self):
        candidate = self.prepare()['pending_candidates'][0]['candidate_id']
        lock = self.root / '.collection.lock'
        lock.write_text('existing owner', encoding='utf-8')
        with self.assertRaises(FileExistsError):
            collection.bind(self.root, candidate, 'one')
        self.assertEqual(lock.read_text(), 'existing owner')
        lock.unlink()  # This fixture owns this synthetic lock.
        state, _, _ = collection.load_state(self.root)
        p = self.root / state['runs'][0]['turns'][0]['prompt']
        p.write_text('{}', encoding='utf-8')
        with self.assertRaises(ValueError):
            collection.bind(self.root, candidate, 'one')

    def test_unpinned_input_and_escaped_input_canary_are_rejected_before_copy(self):
        path = self.manifest['cases'][0]['arms']['baseline']['input']
        original_hash = self.manifest['frozen_files'].pop(path)
        self.freeze()
        with self.assertRaises(ValueError):
            self.prepare()
        self.assertFalse(self.root.exists())
        self.manifest['frozen_files'][path] = original_hash
        self.write(self.corpus / path, {'source': 'JUDGE_ONLY_CANARY'})
        p = self.corpus / path
        p.write_bytes(p.read_bytes().replace(b'JUDGE', b'\\u004aUDGE'))
        self.manifest['frozen_files'][path] = self.sha(p)
        self.freeze()
        with self.assertRaises(ValueError):
            self.prepare()

    def test_judge_task_oracle_drift_and_false_guidance_shape_are_rejected(self):
        self.finish_candidates()
        manifest, oracle = evaluation.load(self.root)
        prompt_path = self.root / 'judge/prompt.json'
        original = prompt_path.read_bytes()
        for field in ['tasks', 'oracle']:
            prompt = json.loads(original)
            prompt[field] = {}
            self.write(prompt_path, prompt)
            with self.assertRaises(ValueError):
                evaluation.results(self.root, manifest, oracle, self.judgments())
        prompt_path.write_bytes(original)
        judgments = self.judgments()
        judgments[0]['false_guidance'] = 'not a list'
        with self.assertRaises(ValueError):
            evaluation.results(self.root, manifest, oracle, judgments)
        self.write(self.response, judgments)
        with self.assertRaises(ValueError):
            collection.judge(self.root, 'independent', self.response)
        self.assertFalse((self.root / 'judgments.json').exists())

    def test_oversized_judge_packet_does_not_accept_partial_final_outputs(self):
        self.prepare()
        state, _, _ = collection.load_state(self.root)
        for n, run in enumerate(state['runs']):
            actor = 'candidate-' + str(n)
            collection.bind(self.root, run['opaque_id'], actor)
            if n < 7:
                collection.record(self.root, run['opaque_id'], actor, self.answer())
            else:
                before = (self.root / collection.STATE).read_bytes()
                with self.assertRaisesRegex(ValueError, 'Combined judge packet'):
                    collection.record(self.root, run['opaque_id'], actor,
                                      self.answer(decision='x' * (1024 * 1024 - 1000)))
                self.assertEqual((self.root / collection.STATE).read_bytes(), before)
        self.assertFalse((self.root / 'runs.json').exists())
        self.assertFalse((self.root / 'judge/prompt.json').exists())
        self.assertFalse((self.root / 'judge/mapping.json').exists())
        self.assertEqual(collection.status(self.root)['completed_candidates'], 7)


if __name__ == '__main__':
    unittest.main()
