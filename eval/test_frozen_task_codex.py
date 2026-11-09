"""Owned fake-process protocol checks; these never invoke a model."""
import json
import os
from pathlib import Path
import sys
import unittest

import frozen_task_codex as adapter
import frozen_task_run as collection
import test_frozen_task_run as protocol


FAKE = r'''
import hashlib,json,os,sys,time,uuid
from pathlib import Path
control=Path(sys.argv[1]);mode=sys.argv[2];args=sys.argv[3:]
if args==['--version']:
 print('codex-cli 0.167.0');raise SystemExit(0)
if args==['mcp','list','--json']:
 print(json.dumps([{'name':'bad.name' if mode=='bad_mcp' else 'engram','transport':{'env':{'TOKEN':'DO_NOT_RETAIN_MCP_CREDENTIAL'}}}]))
 raise SystemExit(0)
assert args[0]=='exec' and '--ephemeral' not in args and '--last' not in args
assert args[args.index('--sandbox')+1]=='read-only'
assert not list(Path.cwd().iterdir()), 'Owned execution cwd must stay neutral'
assert 'mcp_servers.engram.enabled=false' in args
assert not any('mcp_servers."' in arg for arg in args)
for name in ['shell_tool','apps','multi_agent','view_image','browser_use','browser_use_external','computer_use',
             'memories','daemon_auto_start','hooks','plugins','plugin_discovery_prefetch','remote_plugin',
             'skill_search','skill_mcp_dependency_install','external_agent_memory_import']:
 assert 'features.'+name+'=false' in args
assert 'features.skip_host_skill_discovery=true' in args and 'features.omit_skill_descriptions=true' in args
assert 'approval_policy="never"' in args and 'project_doc_max_bytes=0' in args and 'web_search="disabled"' in args
assert '--model' not in args and '-m' not in args and '--ignore-user-config' not in args
if 'suppress_unstable_features_warning=true' not in args:
 print(json.dumps({'type':'thread.started','thread_id':str(uuid.uuid4())}),flush=True)
 print(json.dumps({'type':'item.completed','item':{'type':'error','message':
  'Under-development features enabled: omit_skill_descriptions, skip_host_skill_discovery. '
  'To suppress this warning, set suppress_unstable_features_warning=true'}}),flush=True)
 raise SystemExit(0)
if mode=='early_stdin':
 os.close(0)
 text=json.dumps(dict(decision='Unconsumed packet',plan=[],evidence=[],uncertainties=[],read_requests=[]))
 for event in [{'type':'thread.started','thread_id':str(uuid.uuid4())},{'type':'turn.started'},
               {'type':'item.completed','item':{'type':'agent_message','text':text}},
               {'type':'turn.completed','usage':{'input_tokens':0,'output_tokens':5}}]:
  print(json.dumps(event),flush=True)
 Path(args[args.index('-o')+1]).write_text(text)
 raise SystemExit(0)
raw=sys.stdin.buffer.read();packet=json.loads(raw)
judge='candidates' in packet
resume='resume' in args
session=args[args.index('resume')+1] if resume else str(uuid.uuid4())
log=json.loads(control.read_text()) if control.exists() else []
log.append({'args':args,'session':session,'judge':judge,'resume':resume,'prompt':packet,
            'stdin_sha256':hashlib.sha256(raw).hexdigest(),'cwd':str(Path.cwd()),'pid':os.getpid()})
control.write_text(json.dumps(log))
def emit(value):
 print(json.dumps(value),flush=True)
if mode=='wrong_id' and resume:session=str(uuid.uuid4())
emit({'type':'thread.started','thread_id':session})
emit({'type':'turn.started','model':'test-model','effort':'high'})
if mode=='tool':
 emit({'type':'item.started','item':{'id':'bad','type':'command_execution','command':'unwanted'}})
 time.sleep(60)
if mode=='unknown':emit({'type':'unsupported.event'})
if mode=='timeout':time.sleep(60)
if mode=='flood':
 sys.stdout.write('x'*(2*1024*1024));sys.stdout.flush();time.sleep(60)
if mode=='config_change':
 (control.parent/'home'/'config.toml').write_text('model="different"\n')
if judge:
 assert '--output-schema' not in args
 assert set(packet)=={'instructions','tasks','candidates','oracle'}
 assert all(set(c)=={'candidate_id','case_id','response'} for c in packet['candidates'])
 answer=[dict(candidate_id=c['candidate_id'],criteria={k:'satisfied' for k in packet['oracle']['cases'][c['case_id']]['criteria']},
              false_guidance=[],rationale='Synthetic subprocess protocol only.') for c in packet['candidates']]
else:
 assert '--output-schema' in args
 assert 'JUDGE_ONLY_CANARY' not in raw.decode()
 answer=dict(decision='JUDGE_ONLY_CANARY' if mode=='canary' else 'The supplied fact is supported.',
             plan=[],evidence=['source'],uncertainties=[],read_requests=[] if resume else ['read0'])
text=json.dumps(answer)
Path(args[args.index('-o')+1]).write_bytes((text+ ('different' if mode=='wrong_response' else '\n')).encode())
emit({'type':'item.completed','item':{'id':'final','type':'agent_message','text':text}})
emit({'type':'turn.completed','usage':{'input_tokens':10,'output_tokens':5}})
raise SystemExit(3 if mode=='exit_failure' else 0)
'''


class CodexExecutionTests(unittest.TestCase):
    def setUp(self):
        self.assertIn('ENGRAM_TEST_TMPDIR', os.environ, 'Run subprocess tests in the owned fixture temp directory')
        self.fixture = protocol.FrozenTaskRunTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.base = self.fixture.base
        self.home = self.base / 'home'; self.home.mkdir()
        (self.home / 'config.toml').write_text('model="test-model"\nmodel_reasoning_effort="high"\n', encoding='utf-8')
        self.fake = self.base / 'fake_codex.py'; self.fake.write_text(FAKE, encoding='utf-8')
        self.control = self.base / 'fake-log.json'

    def prepare(self, suffix='run'):
        root = self.base / suffix
        collection.prepare(self.fixture.corpus, root, 'protocol_test', delivery_mode='manual_packet_v1')
        return root

    def execute(self, root, mode='normal', timeout=30):
        return adapter._execute(root, [sys.executable, str(self.fake), str(self.control), mode], timeout, self.home)

    def evidence(self, root):
        return json.loads((root / 'execution/state.json').read_text(encoding='utf-8'))

    def test_full_eight_candidates_exact_resumes_and_fresh_opaque_judge(self):
        (self.home / 'config.toml').write_text('model="unselected-model"\nprofile="selected"\n'
            '[profiles.selected]\nmodel="test-model"\nmodel_reasoning_effort="high"\n', encoding='utf-8')
        root = self.prepare()
        initial, _, _ = collection.load_state(root)
        result = self.execute(root)
        self.assertEqual(result['status'], 'complete')
        state, _, _ = collection.load_state(root)
        evidence = self.evidence(root)
        log = json.loads(self.control.read_text())
        self.assertEqual(len(evidence['session_ids']), 9)
        self.assertEqual(len(set(evidence['session_ids'])), 9)
        self.assertEqual(len(log), 17)
        self.assertEqual(len(evidence['turns']), 17)
        for number, run in enumerate(state['runs']):
            first, second = log[2*number:2*number+2]
            self.assertFalse(first['resume']); self.assertTrue(second['resume'])
            self.assertEqual(first['session'], second['session'])
            self.assertEqual(run['agent'], 'codex:' + first['session'])
            self.assertEqual(len(run['turns']), 2)
            self.assertEqual(run['turns'][1]['read_ids'], ['read0'])
            self.assertEqual(first['stdin_sha256'], initial['artifacts'][initial['runs'][number]['turns'][0]['prompt']])
            self.assertLessEqual(evidence['turns'][2*number]['invoked_at'], run['started_at'])
        self.assertTrue(log[-1]['judge']); self.assertFalse(log[-1]['resume'])
        self.assertEqual(state['judge_agent'], 'codex:' + log[-1]['session'])
        self.assertEqual(evidence['judge_agent'], state['judge_agent'])
        self.assertEqual(evidence['observed_selection'], {'model':'test-model','effort':'high'})
        for turn in evidence['turns']:
            self.assertIn('suppress_unstable_features_warning=true', turn['argv'])
            self.assertEqual(turn['exit_code'], 0)
            self.assertTrue(turn['owned_threads_closed'])
            self.assertTrue(turn['stdin_complete'])
            self.assertIn('accepted_at', turn)
            for name in ('stdin', 'events.jsonl', 'stderr.log', 'response.json'):
                row = turn[name]
                self.assertEqual(collection.digest(Path(row['path']).read_bytes()), row['sha256'])
            self.assertEqual(turn['usage'], {'input_tokens':10,'output_tokens':5})
        for path in (root / 'execution').rglob('*'):
            if path.is_file():
                self.assertNotIn(b'DO_NOT_RETAIN_MCP_CREDENTIAL', path.read_bytes())
        self.assertFalse((root / '.codex-execution.lock').exists())
        before = (root / 'execution/state.json').read_bytes()
        with self.assertRaises(ValueError):
            self.execute(root)
        self.assertEqual((root / 'execution/state.json').read_bytes(), before)

    def test_contamination_wrong_id_exit_and_response_fail_closed(self):
        for mode in ('tool', 'unknown', 'wrong_id', 'exit_failure', 'wrong_response', 'canary', 'flood'):
            with self.subTest(mode=mode):
                root = self.prepare('run-' + mode)
                with self.assertRaises((ValueError, UnicodeError)):
                    self.execute(root, mode)
                state, _, _ = collection.load_state(root)
                evidence = self.evidence(root)
                self.assertEqual(evidence['status'], 'failed')
                self.assertFalse(any('response' in run for run in state['runs']))
                self.assertTrue(evidence['turns'][-1]['owned_threads_closed'])
                self.assertIsNotNone(evidence['turns'][-1]['exit_code'])
                self.assertFalse((root / '.codex-execution.lock').exists())
                saved = (root / 'execution/state.json').read_bytes()
                with self.assertRaises((ValueError, FileExistsError)):
                    self.execute(root, mode)
                self.assertEqual((root / 'execution/state.json').read_bytes(), saved)

    def test_explicit_timeout_terminates_only_owned_child_and_reaps_threads(self):
        root = self.prepare()
        with self.assertRaises(TimeoutError):
            self.execute(root, 'timeout', timeout=3)
        evidence = self.evidence(root)
        turn = evidence['turns'][-1]
        self.assertTrue(turn['terminated_owned_child'])
        self.assertTrue(turn['owned_threads_closed'])
        self.assertIsNotNone(turn['exit_code'])
        self.assertEqual(evidence['status'], 'failed')

    def test_config_change_and_unusable_mcp_name_never_advance_collection(self):
        for mode in ('bad_mcp', 'config_change'):
            with self.subTest(mode=mode):
                root = self.prepare('run-' + mode)
                with self.assertRaises(ValueError):
                    self.execute(root, mode)
                state, _, _ = collection.load_state(root)
                self.assertFalse(any('response' in run for run in state['runs']))
                self.assertEqual(self.evidence(root)['status'], 'failed')

    def test_existing_lock_and_manual_started_run_are_not_replaced(self):
        root = self.prepare()
        lock = root / '.codex-execution.lock'; lock.write_text('other owner')
        with self.assertRaises(FileExistsError):
            self.execute(root)
        self.assertEqual(lock.read_text(), 'other owner')
        lock.unlink()  # This fixture owns its synthetic lock.
        state, _, _ = collection.load_state(root)
        collection.bind(root, state['runs'][0]['opaque_id'], 'manual-agent')
        with self.assertRaises(ValueError):
            self.execute(root)
        self.assertFalse((root / 'execution').exists())

    def test_instruction_override_fails_before_any_child_and_is_retained(self):
        for key in ('developer_instructions', 'model_instructions_file', 'instructions_file', 'instructions'):
            with self.subTest(key=key):
                (self.home / 'config.toml').write_text('model="test-model"\n' + key + '="PRIVATE_OVERRIDE"\n', encoding='utf-8')
                root = self.prepare('run-' + key)
                with self.assertRaisesRegex(ValueError, 'instruction override'):
                    self.execute(root)
                self.assertFalse(self.control.exists())
                self.assertEqual(self.evidence(root)['status'], 'failed')
                self.assertNotIn('PRIVATE_OVERRIDE', (root / 'execution/state.json').read_text())

    def test_zero_exit_final_reply_cannot_hide_incomplete_stdin_write(self):
        for case in self.fixture.manifest['cases']:
            for arm in case['arms'].values():
                name = arm['input']
                self.fixture.write(self.fixture.corpus / name, {'source':'x' * (256*1024)})
                self.fixture.manifest['frozen_files'][name] = self.fixture.sha(self.fixture.corpus / name)
        self.fixture.freeze()
        root = self.prepare()
        with self.assertRaises(ValueError):
            self.execute(root, 'early_stdin')
        state, _, _ = collection.load_state(root)
        self.assertFalse(any('response' in run for run in state['runs']))
        turn = self.evidence(root)['turns'][-1]
        self.assertFalse(turn['stdin_complete'])
        self.assertTrue(turn['owned_threads_closed'])


if __name__ == '__main__':
    unittest.main()
