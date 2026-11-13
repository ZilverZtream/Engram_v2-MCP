"""Current-check workflow tests with synthetic producer replies and judgments."""
import contextlib
import io
import json
import sys
import unittest
from unittest.mock import patch

import frozen_task_capture as capture
import frozen_task_check as check
import frozen_task_run as collection
import test_frozen_task_capture as producer_protocol


class CurrentQualityCheckTests(unittest.TestCase):
    def setUp(self):
        self.fixture = producer_protocol.CurrentProducerTests('runTest')
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.destination = self.fixture.root / 'check'
        self.executions = []
        self.quality_failure = None
        self.change_build = False
        for case in self.fixture.spec['cases']:
            case['tool_requests'] = [dict(name='get_change_set', arguments={'project_id': 'project'})]
        self.fixture.write(self.fixture.specdir / 'spec.json', self.fixture.spec)
        original = capture.prepare_current

        def current(spec, destination, pins):
            return original(spec, destination, pins, transport=self.fixture.transport,
                            identify=lambda pid: dict(self.fixture.identity))

        self.capture_patch = patch.object(capture, 'prepare_current', side_effect=current)
        self.capture_patch.start(); self.addCleanup(self.capture_patch.stop)
        self.execute_patch = patch.object(check.executor, 'execute', side_effect=self.execute)
        self.execute_mock = self.execute_patch.start(); self.addCleanup(self.execute_patch.stop)

    def execute(self, root, executable, timeout):
        self.executions.append(root)
        state, _, _ = collection.load_state(root)
        self.assertEqual(state['producer_kind'], 'current_producer')
        response = self.fixture.root / 'response.json'
        for number, run in enumerate(state['runs']):
            actor = 'synthetic-candidate-' + str(number)
            collection.bind(root, run['opaque_id'], actor)
            self.fixture.write(response, dict(decision='Synthetic answer', plan=[],
                evidence=['source'], uncertainties=[], read_requests=[]))
            collection.record(root, run['opaque_id'], actor, response)
        mapping = json.loads((root / 'judge/mapping.json').read_text())
        judgments = []
        for candidate, entry in mapping.items():
            assisted_failure = self.quality_failure if entry['arm'] == 'assisted' else None
            judgments.append(dict(candidate_id=candidate,
                criteria={'correct': 'not_satisfied' if assisted_failure == 'criterion' else 'satisfied'},
                false_guidance=['Synthetic false advice'] if assisted_failure == 'false_guidance' else [],
                rationale='Synthetic quality gate test, not model evaluation.'))
        self.fixture.write(response, judgments)
        result = collection.judge(root, 'independent-synthetic-judge', response)
        if self.change_build:
            self.fixture.binary.write_bytes(b'a different build')
        return result

    def command(self):
        arguments = ['frozen_task_run.py', 'check-current', str(self.fixture.specdir),
                     str(self.destination), '--pins', str(self.fixture.pinfile),
                     '--codex-executable', str(self.fixture.binary)]
        with patch.object(sys, 'argv', arguments), contextlib.redirect_stdout(io.StringIO()) as output:
            code = collection.main()
        return code, json.loads(output.getvalue())

    def test_current_command_captures_executes_and_cannot_reuse_saved_green_results(self):
        code, result = self.command()
        self.assertEqual(code, 0)
        self.assertEqual(result['status'], 'current_producer_quality_passed')
        self.assertEqual(result['producer']['binary_sha256'], self.fixture.pins['binary_sha256'])
        self.assertEqual(len(result['quality']['metrics']), 8)
        self.assertEqual(len(self.executions), 1)
        self.assertTrue(self.fixture.pipe.closed)
        self.assertEqual(sum(params.get('name') == 'get_change_set' for _, params in self.fixture.calls), 4)
        before = (self.destination / 'current-check.json').read_bytes()
        with self.assertRaises(FileExistsError):
            self.command()
        self.assertEqual(len(self.executions), 1)
        self.assertEqual((self.destination / 'current-check.json').read_bytes(), before)

    def test_false_guidance_fails_the_public_command_despite_complete_execution(self):
        self.quality_failure = 'false_guidance'
        code, result = self.command()
        self.assertEqual(code, 1)
        self.assertEqual(result['status'], 'current_producer_quality_failed')
        self.assertEqual(result['execution']['status'], 'complete')
        self.assertEqual(len(result['quality']['failed_cases']), 4)

    def test_unmet_criteria_fail_the_public_command(self):
        self.quality_failure = 'criterion'
        code, result = self.command()
        self.assertEqual(code, 1)
        self.assertEqual(len(result['quality']['failed_cases']), 4)

    def test_capture_failure_stops_before_models_and_retains_failed_stage(self):
        self.fixture.tool_error = True
        with self.assertRaises(ValueError):
            self.command()
        self.execute_mock.assert_not_called()
        state = json.loads((self.destination / 'current-check.json').read_text())
        self.assertEqual((state['status'], state['stage']), ('failed', 'capture'))
        self.assertNotIn('quality', state)
        self.assertTrue(self.fixture.pipe.closed)

    def test_replacing_the_build_during_evaluation_cannot_pass_current_verification(self):
        self.change_build = True
        with self.assertRaisesRegex(ValueError, 'Installed build changed'):
            self.command()
        state = json.loads((self.destination / 'current-check.json').read_text())
        self.assertEqual((state['status'], state['stage']), ('failed', 'quality'))
        self.assertNotIn('quality', state)


if __name__ == '__main__':
    unittest.main()
