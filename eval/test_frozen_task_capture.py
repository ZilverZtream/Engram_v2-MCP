"""Synthetic producer capture protocol checks, never tool-quality evidence."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import frozen_task_capture as capture
import frozen_task_eval as evaluation
import frozen_task_run as collection


class CurrentProducerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='frozen-current-', dir=os.environ.get('ENGRAM_TEST_TMPDIR'))
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.specdir = self.root / 'spec'
        self.specdir.mkdir()
        self.source = self.root / 'source'
        self.source.mkdir()
        (self.source / 'entry.vb').write_bytes(b'Function Entry()\nEnd Function\n')
        self.binary = self.root / 'engram.exe'
        self.binary.write_bytes(b'synthetic binary')
        self.build = self.root / 'build.json'
        self.write(self.build, {'synthetic': 'build'})
        self.primary = self.root / '.engram.primary'
        self.primary.write_text('pid=123\nstarted_at_ms=456\nprotocol_version=1\nsocket=\\\\.\\pipe\\engram-test\nversion=1\n')
        self.pins = dict(binary=str(self.binary), binary_sha256=capture.file_hash(self.binary),
                         build_manifest=str(self.build), build_manifest_sha256=capture.file_hash(self.build),
                         primary_metadata=str(self.primary), primary_pid=123, primary_started_at_ms=456,
                         source_root=str(self.source), project_id='project', active_generation=5)
        self.pinfile = self.root / 'pins.json'
        self.write(self.pinfile, self.pins)
        self.write(self.specdir / 'base.json', {'file': 'entry.vb', 'text': 'Function Entry()'})
        self.write(self.specdir / 'read.json', {'file': 'entry.vb', 'text': 'End Function'})
        self.spec = dict(version=1, oracle='oracle.json',
                         source_files={'entry.vb': capture.file_hash(self.source / 'entry.vb')}, cases=[])
        for number, category in enumerate(sorted(evaluation.CATEGORIES)):
            self.spec['cases'].append(dict(id='case' + str(number), category=category,
                task='Explain the entry.', base_inputs=['base.json'], available_reads={'body': 'read.json'},
                tool_requests=[dict(name='get_codebase_overview', arguments={'project_id': 'project', 'scope': 'entry.vb'})]))
        self.write(self.specdir / 'oracle.json', dict(judge_only_canary='SECRET_ORACLE_CANARY',
            cases={c['id']: {'criteria': {'correct': 'Explain entry.'}} for c in self.spec['cases']}))
        self.destination = self.root / 'capture'
        self.calls = []
        self.on_call = lambda request: None
        self.tool_error = False
        self.close_error = False
        self.generation_after = 5
        self.case_generation_overrides = {}
        self.visible_tools = capture.TOOLS | {'get_index_freshness'}
        self.identity = dict(binary=str(self.binary), created_ticks=789)
        outer = self

        class FakePipe:
            def __init__(self, socket, pid, emit):
                self.emit = emit
                self.closed = False
                outer.pipe = self

            def rpc(self, method, params):
                outer.calls.append((method, params))
                self.emit('rpc/fake-' + str(len(outer.calls)) + '.json', collection.encoded(dict(method=method, params=params)))
                if method == 'initialize':
                    return {'protocolVersion': '2024-11-05'}
                if method == 'tools/list':
                    return {'tools': [{'name': t} for t in outer.visible_tools]}
                outer.on_call(params)
                if params['name'] == 'get_index_freshness':
                    project_id = params['arguments']['project_id']
                    if 'case_projects' in outer.pins:
                        project = next(p for p in outer.pins['case_projects'].values()
                                       if p['project_id'] == project_id)
                        source = project['source_root']
                        generation = outer.case_generation_overrides.get(project_id, project['active_generation'])
                    else:
                        source = outer.source
                        generation = outer.generation_after if len(outer.calls) > 3 else 5
                    text = (f'project_id: {project_id}\ndirectory: {source}\nactive_generation: {generation}\n'
                            'generation_complete: true (1 of 1)\nsource_index_format: current (version 7)\n')
                else:
                    text = 'Complete tool output\n' + 'line\n' * 4000
                return {'content': [{'type': 'text', 'text': text}], 'isError': outer.tool_error}

            def initialized(self):
                pass

            def close(self):
                self.closed = True
                if outer.close_error:
                    raise OSError('synthetic observer close failure')
        self.transport = FakePipe

    @staticmethod
    def write(path, value):
        path.write_bytes(collection.encoded(value))

    def run_capture(self):
        self.write(self.specdir / 'spec.json', self.spec)
        return capture.prepare_current(self.specdir, self.destination, self.pinfile,
                    transport=self.transport, identify=lambda pid: dict(self.identity))

    def assert_failed(self, message):
        with self.assertRaisesRegex(ValueError, message):
            self.run_capture()
        self.assertFalse((self.destination / 'freeze_record.json').exists())
        if self.destination.exists():
            self.assertTrue((self.destination / 'capture-failure.json').exists())

    def test_captures_complete_text_and_reuses_existing_eight_arm_protocol(self):
        self.assertEqual(self.run_capture()['producer_requests'], 4)
        manifest, _ = evaluation.load(self.destination)
        self.assertEqual(manifest['producer_kind'], 'current_producer')
        baseline = evaluation.packet(self.destination, manifest, 'case0', 'baseline', [])['source_inputs']
        assisted = evaluation.packet(self.destination, manifest, 'case0', 'assisted', [])['source_inputs']
        self.assertEqual(baseline['source_inputs'], assisted['source_inputs'])
        self.assertNotIn('tool_responses', baseline)
        response = assisted['tool_responses'][0]
        self.assertEqual(response['response']['content'][0]['text'].count('line\n'), 4000)
        self.assertEqual(response['response_sha256'], collection.digest(collection.encoded(response['response'])))
        self.assertEqual(response['response_bytes'], len(collection.encoded(response['response'])))
        for arm in ('baseline', 'assisted'):
            self.assertEqual(evaluation.packet(self.destination, manifest, 'case0', arm, ['body'])['read_results'],
                             {'body': {'file': 'entry.vb', 'text': 'End Function'}})
        run = self.root / 'run'
        self.assertEqual(len(collection.prepare(self.destination, run, 'protocol_test')['pending_candidates']), 8)
        self.assertTrue(self.pipe.closed)

    def test_oracle_spec_and_source_frozen_before_any_query(self):
        def check(request):
            self.assertTrue((self.destination / 'recipe/spec.json').exists())
            self.assertTrue((self.destination / 'recipe/oracle.json').exists())
            self.assertTrue((self.destination / 'provenance/before.json').exists())
        self.on_call = check
        self.run_capture()

    def test_new_capture_explains_per_turn_and_total_reads_and_keeps_followup_choices_visible(self):
        self.run_capture()
        manifest, _ = evaluation.load(self.destination)
        initial = evaluation.packet(self.destination, manifest, 'case0', 'baseline', [])
        self.assertIn('three IDs from available_reads per turn', initial['instructions'])
        self.assertIn('eight reads', initial['instructions'])
        followup = evaluation.packet(self.destination, manifest, 'case0', 'baseline', ['body'])
        self.assertEqual(followup['available_reads'], ['body'])
        self.assertIn('three more', followup['instructions'])
        self.assertIn('eight reads', followup['instructions'])
        # Frozen wording, rather than later code, defines this sealed trial.
        with patch.dict(evaluation.READ_INSTRUCTIONS, initial='later wording', continuation='later continuation'):
            self.assertEqual(evaluation.packet(self.destination, manifest, 'case0', 'baseline', []), initial)
            self.assertEqual(evaluation.packet(self.destination, manifest, 'case0', 'baseline', ['body']), followup)

    def test_existing_sealed_corpus_without_new_wording_keeps_original_packets(self):
        self.run_capture()
        manifest, _ = evaluation.load(self.destination)
        manifest.pop('read_instructions')
        initial = evaluation.packet(self.destination, manifest, 'case0', 'baseline', [])
        self.assertEqual(initial['instructions'], evaluation.INSTRUCTIONS)
        followup = evaluation.packet(self.destination, manifest, 'case0', 'baseline', ['body'])
        self.assertEqual(followup, dict(case_id='case0',
            instructions='Use these frozen read results to finish the prior task; return the same JSON shape.',
            read_results={'body': {'file': 'entry.vb', 'text': 'End Function'}}))

    def test_mutating_request_refused_without_connecting(self):
        self.spec['cases'][0]['tool_requests'][0]['name'] = 'update_project'
        self.assert_failed('read-only')
        self.assertEqual(self.calls, [])

    def test_original_workflow_requests_work_without_unrequested_core_tools(self):
        names = ['get_change_set', 'get_change_set', 'search_history', 'derive_test_matrix']
        for case, name in zip(self.spec['cases'], names):
            case['tool_requests'] = [dict(name=name, arguments={'project_id': 'project'})]
        self.visible_tools = set(names) | {'get_index_freshness'}
        self.assertEqual(self.run_capture()['producer_requests'], 4)
        actual = [params['name'] for method, params in self.calls
                  if method == 'tools/call' and params['name'] != 'get_index_freshness']
        self.assertEqual(actual, names)
        manifest, _ = evaluation.load(self.destination)
        for case, name in zip(manifest['cases'], names):
            inputs = evaluation.packet(self.destination, manifest, case['id'], 'assisted', [])
            self.assertEqual(inputs['source_inputs']['tool_responses'][0]['request']['name'], name)

    def isolated_case_sources(self):
        self.spec['version'] = 2
        self.spec['source_files'] = {}
        self.pins['case_projects'] = {}
        for number, case in enumerate(self.spec['cases']):
            key = case['id']
            source = self.root / ('source-' + key)
            source.mkdir()
            (source / 'entry.vb').write_text('Historical source for ' + key)
            self.spec['source_files'][key] = {'entry.vb': capture.file_hash(source / 'entry.vb')}
            self.pins['case_projects'][key] = dict(source_root=str(source), project_id=key,
                                                  active_generation=number + 1)
            case['tool_requests'] = [dict(name='derive_test_matrix', arguments={'project_id': key})]
        self.write(self.pinfile, self.pins)

    def test_historical_cases_capture_distinct_sources_and_verify_each_generation_twice(self):
        self.isolated_case_sources()
        self.run_capture()
        freshness = [p['arguments']['project_id'] for method, p in self.calls
                     if method == 'tools/call' and p['name'] == 'get_index_freshness']
        self.assertEqual(sorted(freshness), sorted(list(self.pins['case_projects']) * 2))
        manifest, _ = evaluation.load(self.destination)
        for case in manifest['cases']:
            assisted = evaluation.packet(self.destination, manifest, case['id'], 'assisted', [])
            self.assertEqual(assisted['source_inputs']['tool_responses'][0]['request']['arguments']['project_id'],
                             case['id'])
        before = json.loads((self.destination / 'provenance/before.json').read_text())
        self.assertEqual(before['source_files'], self.spec['source_files'])
        self.assertTrue(self.pipe.closed)

    def test_case_request_cannot_use_another_historical_projects_later_code(self):
        self.isolated_case_sources()
        self.spec['cases'][0]['tool_requests'][0]['arguments']['project_id'] = self.spec['cases'][1]['id']
        self.assert_failed('pinned case project')
        self.assertEqual(self.calls, [])

    def test_unqueried_historical_source_drift_prevents_sealing(self):
        self.isolated_case_sources()
        last = self.pins['case_projects'][self.spec['cases'][-1]['id']]
        def change(request):
            if request['name'] == 'derive_test_matrix':
                (Path(last['source_root']) / 'entry.vb').write_bytes(b'later code')
        self.on_call = change
        self.assert_failed('source evidence changed')
        self.assertTrue(self.pipe.closed)

    def test_stale_case_generation_prevents_model_inputs(self):
        self.isolated_case_sources()
        self.case_generation_overrides[self.spec['cases'][2]['id']] = 999
        self.assert_failed('generation differs')
        self.assertFalse(any(method == 'tools/call' and p['name'] == 'derive_test_matrix'
                             for method, p in self.calls))

    def test_historical_project_identity_cannot_be_reused_for_two_revisions(self):
        self.isolated_case_sources()
        projects = list(self.pins['case_projects'].values())
        projects[1]['project_id'] = projects[0]['project_id']
        self.write(self.pinfile, self.pins)
        self.assert_failed('distinct pinned projects')
        self.assertEqual(self.calls, [])

    def test_missing_requested_tool_fails_before_producer_calls(self):
        self.spec['cases'][0]['tool_requests'][0]['name'] = 'search_history'
        self.visible_tools = {'get_codebase_overview', 'get_index_freshness'}
        self.assert_failed('Incomplete installed tools/list')
        self.assertFalse(any(method == 'tools/call' for method, _ in self.calls))
        self.assertTrue(self.pipe.closed)

    def test_prepared_source_drift_refused_before_query(self):
        (self.source / 'entry.vb').write_bytes(b'changed')
        self.assert_failed('source evidence changed')
        self.assertEqual(self.calls, [])

    def test_source_change_during_query_retains_failure_and_closes_connection(self):
        def change(request):
            if request['name'] == 'get_codebase_overview':
                (self.source / 'entry.vb').write_bytes(b'changed during query')
        self.on_call = change
        self.assert_failed('source evidence changed')
        self.assertTrue(self.pipe.closed)
        self.assertTrue((self.destination / 'producer/case0-0.json').exists())

    def test_generation_change_refuses_freeze(self):
        self.generation_after = 6
        self.assert_failed('generation differs')

    def test_observer_close_failure_cannot_leave_an_admissible_freeze(self):
        self.close_error = True
        with self.assertRaisesRegex(OSError, 'close failure'):
            self.run_capture()
        self.assertFalse((self.destination / 'freeze_record.json').exists())
        self.assertTrue((self.destination / 'capture-failure.json').exists())

    def test_current_failure_receipt_cannot_be_prepared_as_success(self):
        self.run_capture()
        self.write(self.destination / 'capture-failure.json', {'status': 'failed'})
        with self.assertRaisesRegex(ValueError, 'capture is failed'):
            collection.prepare(self.destination, self.root / 'run', 'protocol_test')

    def test_process_restart_refuses_freeze(self):
        def restart(request):
            if request['name'] == 'get_codebase_overview':
                self.identity['created_ticks'] += 1
        self.on_call = restart
        self.assert_failed('metadata drift')

    def test_error_tool_result_is_not_success_even_when_text_looks_valid(self):
        self.tool_error = True
        self.assert_failed('isError')

    def test_installed_manifest_change_refuses_freeze(self):
        def change(request):
            if request['name'] == 'get_codebase_overview':
                self.build.write_bytes(b'changed manifest')
        self.on_call = change
        self.assert_failed('manifest hash drift')

    def test_existing_destination_and_escaping_input_are_refused(self):
        self.destination.mkdir()
        with self.assertRaisesRegex(ValueError, 'fresh external destination'):
            self.run_capture()
        self.destination.rmdir()  # Empty fixture owned by this test.
        self.spec['cases'][0]['base_inputs'] = ['../pins.json']
        self.assert_failed('normalized relative')
        self.assertEqual(self.calls, [])

    def test_pipe_observation_waits_do_not_resend_pending_request(self):
        reply = b'{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}\n'
        writes, observations = [], []

        class Stream:
            def write(self, raw):
                writes.append(raw)
                return len(raw)

            def read(self, count):
                return reply

        class Api:
            def PeekNamedPipe(self, handle, buffer, size, read, available, left):
                observations.append(True)
                available._obj.value = len(reply) if len(observations) == 3 else 0
                return True

        pipe = capture.InstalledPipe.__new__(capture.InstalledPipe)
        pipe.stream, pipe.api, pipe.handle = Stream(), Api(), None
        pipe.emit, pipe.deadline, pipe.sequence = lambda name, raw: None, 180, 0
        with patch.object(capture.time, 'sleep') as sleep:
            self.assertEqual(pipe.rpc('tools/list', {}), {'tools': []})
        self.assertEqual(len(writes), 1)
        self.assertEqual(sleep.call_count, 2)
        self.assertEqual(json.loads(writes[0])['id'], 1)


if __name__ == '__main__':
    unittest.main()
