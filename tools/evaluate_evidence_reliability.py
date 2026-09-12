"""Repeatable MCP acceptance against isolated snapshots of supplied repositories.

No source-specific rules or credentials. Outputs include exact commits, binary
digest, assertions and timings. Synthetic fixture checks are labelled separately
from held-out repository retrieval checks. Run with --help for inputs.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import threading
import time


class Client:
    def __init__(self, binary, output, config=None):
        self.binary, self.output, self.config = binary, output, config

    def __enter__(self):
        env = os.environ.copy()
        args = [str(self.binary)]
        if self.config:
            env['ENGRAM_CONFIG_PATH'] = str(self.config)
            args.append('--no-multi-client')
        self.log = (self.output / 'mcp-stderr.log').open('a', encoding='utf-8')
        self.process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=self.log, text=True, encoding='utf-8', env=env,
            creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0))
        self.messages = queue.Queue()
        def read():
            for line in self.process.stdout:
                self.messages.put(json.loads(line))
            self.messages.put(None)
        threading.Thread(target=read, daemon=True).start()
        self.number = 0
        self.rpc('initialize', {'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'evidence-reliability-eval','version':'1'}})
        self.send({'jsonrpc':'2.0','method':'notifications/initialized'})
        return self

    def send(self, value):
        self.process.stdin.write(json.dumps(value)+'\n')
        self.process.stdin.flush()

    def rpc(self, method, params):
        self.number += 1
        self.send({'jsonrpc':'2.0','id':self.number,'method':method,'params':params})
        deadline = time.monotonic()+900
        while True:
            message = self.messages.get(timeout=max(0.1, deadline-time.monotonic()))
            if message is None:
                raise RuntimeError('MCP disconnected')
            if message.get('id') == self.number:
                if 'error' in message:
                    raise RuntimeError(str(message['error']))
                return message['result']

    def call(self, name, arguments):
        result = self.rpc('tools/call', {'name':name,'arguments':arguments})
        text = '\n'.join(c.get('text','') for c in result.get('content',[]))
        if result.get('isError'):
            raise RuntimeError(text)
        return text

    def __exit__(self, *_):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(timeout=10)
        self.log.close()


def git(root, *arguments):
    return subprocess.check_output(['git','-C',str(root),*arguments], text=True, encoding='utf-8').strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repo', type=Path, action='append', default=[])
    parser.add_argument('--live', action='store_true', help='Use deployed daemon/config; otherwise isolated FTS-only config')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    copies = output/'repositories'; copies.mkdir(exist_ok=True)
    config = output/'engram-eval.json'
    config.write_text(json.dumps({'data_dir':str(output/'data'),'allowed_roots':[str(copies)],
        'embedding_backend':'fts_only','multi_client':False,'max_concurrent_jobs':1,
        'advertise_all_tools':True}), encoding='utf-8')
    repos = []
    for n, source in enumerate(args.repo):
        dest = copies/f'reference-{n}'
        if not dest.exists():
            subprocess.run(['git','-c','core.autocrlf=false','clone','--local','--no-hardlinks','--quiet',str(source.resolve()),str(dest)],check=True)
        repos.append(dest)
    fixture = copies/'portable-fixture'; fixture.mkdir(exist_ok=True)
    if not (fixture/'.git').exists():
        subprocess.run(['git','init','-q',str(fixture)],check=True)
        (fixture/'Rules.vb').write_bytes(b'Public Class Rules\r\n Public Sub Save(value As Integer)\r\n End Sub\r\n Public Sub Save(value As String)\r\n End Sub\r\nEnd Class\r\n')
        (fixture/'Rules.cs').write_text('public class Rules { public int Save(int value) { return value; } }\n')
        git(fixture,'add','.')
        git(fixture,'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-qm','Merged PR 7: portable fixture')
    rows = []
    with Client(args.binary, output, None if args.live else config) as client:
        schemas = {t['name']:t for t in client.rpc('tools/list',{})['tools']}
        (output/'schemas.json').write_text(json.dumps(schemas,indent=2),encoding='utf-8')
        assert 'start_line' in schemas['find_tests_for_method']['inputSchema']['properties']
        assert {'record_review_decisions','get_review_decisions'} <= schemas.keys()
        def call(label, tool, arguments):
            start=time.monotonic()
            result=client.call(tool,arguments)
            (output/(label+'.txt')).write_text(result,encoding='utf-8')
            row={'label':label,'tool':tool,'seconds':round(time.monotonic()-start,3)}
            rows.append(row); print(json.dumps(row),flush=True)
            return result
        def rejected(label, tool, arguments):
            try:
                call(label, tool, arguments)
            except RuntimeError as error:
                (output/(label+'.txt')).write_text(str(error),encoding='utf-8')
                rows.append({'label':label,'expected_rejection':True})
                return
            raise AssertionError(label+' unexpectedly succeeded')
        for n, root in enumerate([*repos,fixture]):
            label=f'repo-{n}'
            indexed=call(label+'-index','index_project',{'directory':str(root),'project_name':label,'project_type':'general','wait':True})
            match=re.search(r'project_id[:"\s]+([0-9a-f-]{36})',indexed)
            if not match:
                projects=call(label+'-projects','list_projects',{})
                raise AssertionError('Index response missing project ID; inspect '+label+'-index.txt and projects')
            pid=match[1]
            tracked=git(root,'ls-files').splitlines()
            choices=[p for p in tracked if p.endswith(('.cs','.rs','.vb')) and 'test' not in p.lower()]
            assert choices
            path=next((p for p in choices if p.endswith('.rs')),choices[0])
            content=(root/path).read_text(encoding='utf-8-sig')
            tokens=re.findall(r'\b[A-Za-z_][A-Za-z_0-9]{9,}\b',content) or re.findall(r'\b[A-Za-z_][A-Za-z_0-9]{3,}\b',content)
            assert tokens
            token=next((t for t in tokens if t not in {'System','Collections','SystemTextJson','Serializable'}),tokens[0])
            response=json.loads(call(label+'-grep','grep_project',{'project_id':pid,'pattern':token,'path_prefix':path,'output_json':True}))
            matches=response['matches']; assert matches and matches[0].get('doc_id')
            chunk=call(label+'-chunk','get_chunk',{'project_id':pid,'doc_id':matches[0]['doc_id']})
            # grep uses smart case: a lowercase query can return an uppercase
            # identifier. Verify both the case-insensitive query and exact hit.
            assert token.casefold() in chunk.casefold()
            assert matches[0]['line_text'].strip() in chunk
            matrix=call(label+'-matrix','derive_test_matrix',{'project_id':pid,'files':[path]})
            assert 'STALE graph axes withheld' not in matrix
            review=json.loads(call(label+'-coverage','pre_commit_review',{'project_id':pid,'diff':f'diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-old\n+new\n','output_json':True}))
            assert review['coverage']['textual_diff_files']==[path]
            assert review['coverage']['test_execution']=='not_run'
            assert review['coverage']['source_snapshot_complete']
            rows.append({'repository':str(root),'commit':git(root,'rev-parse','HEAD'),'kind':'synthetic' if root==fixture else 'held_out','retrieval_assertions_passed':True})
            if root==fixture:
                event={'event_id':'first','finding_id':'fixture-1','supersedes':None,'kind':'claimed_fix','source_url':'https://example.invalid/reviews/1','author':'fixture reviewer','recorded_at':'2026-09-09T12:00:00Z','rationale':'Resolved by comment only','verification':None}
                call('fixture-record','record_review_decisions',{'project_id':pid,'review_id':'PR-7','decisions':[event]})
                decision=json.loads(call('fixture-read','get_review_decisions',{'project_id':pid,'review_id':'PR-7'}))
                assert decision['current'][0]['effective_status']=='claimed_fix'
                assert not decision['automatic_finding_suppression']
                crlf=call('fixture-crlf-matrix','derive_test_matrix',{'project_id':pid,'files':['Rules.vb']})
                assert 'STALE graph axes withheld' not in crlf
                original=(root/'Rules.vb').read_bytes()
                try:
                    (root/'Rules.vb').write_bytes(original.replace(b'value As Integer',b'value As Long'))
                    stale=call('fixture-stale-matrix','derive_test_matrix',{'project_id':pid,'files':['Rules.vb']})
                    assert 'STALE graph axes withheld' in stale
                finally:
                    (root/'Rules.vb').write_bytes(original)
                rejected('fixture-overload-ambiguous','find_tests_for_method',{'project_id':pid,'method_name':'Save','file_path':'Rules.vb'})
                rejected('fixture-overload-conflict','find_tests_for_method',{'project_id':pid,'method_name':'Save','file_path':'Rules.vb','start_line':99})
                for line in [2,4]:
                    selection=json.loads(call('fixture-overload-'+str(line),'find_tests_for_method',{'project_id':pid,'method_name':'Save','file_path':'Rules.vb','start_line':line,'output_json':True}))
                    assert selection['target_start_line']==line
                invalid={**event,'event_id':'second','supersedes':'first','kind':'verified_fix'}
                rejected('fixture-verification-required','record_review_decisions',{'project_id':pid,'review_id':'PR-7','decisions':[invalid]})
                verified={**invalid,'verification':{'commit':git(root,'rev-parse','HEAD'),'check':'synthetic external check attestation','evidence_url':'https://example.invalid/evidence','artifact_sha256':'a'*64,'verifier':'fixture'}}
                call('fixture-attestation','record_review_decisions',{'project_id':pid,'review_id':'PR-7','decisions':[verified]})
                current=json.loads(call('fixture-attestation-current','get_review_decisions',{'project_id':pid,'review_id':'PR-7'}))
                assert current['current'][0]['effective_status']=='externally_verified_fix_at_current_clean_head'
                try:
                    (root/'Rules.vb').write_bytes(original+b"' edited\r\n")
                    stale=json.loads(call('fixture-attestation-dirty','get_review_decisions',{'project_id':pid,'review_id':'PR-7'}))
                    assert stale['current'][0]['effective_status']=='verification_not_current'
                finally:
                    (root/'Rules.vb').write_bytes(original)
                call('fixture-history-ingest','ingest_merged_prs',{'project_id':pid,'max_commits':20})
                history=call('fixture-history-decisions','find_merged_work',{'project_id':pid,'story':'portable fixture'})
                assert 'externally_verified_fix_at_current_clean_head' in history
                replay=call('fixture-history-replay','find_merged_work',{'project_id':pid,'story':'portable fixture','merged_before':'2100-01-01'})
                assert 'Review decisions omitted for point-in-time replay' in replay
                assert 'synthetic external check attestation' not in replay
    report={'binary_sha256':hashlib.sha256(args.binary.read_bytes()).hexdigest(),'live':args.live,'passed':True,'results':rows,
        'limits':'Deterministic task checks, not accuracy percentages or concurrent-load benchmarks. Reference code is never executed.'}
    (output/'results.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
    print('PASS',flush=True)


if __name__=='__main__':
    main()
