import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CLIENT = Path(__file__).with_name('replay_mcp_call.py')


class ReplayClientTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.schema = {'probe':{'name':'probe','description':'Unicode → 漢字', 'inputSchema':{
            'type':'object','required':['story'],'additionalProperties':False,
            'properties':{'story':{'type':'string'},'context':{'anyOf':[{'type':'string'},{'type':'null'}]}}}}}
        (self.root/'schemas.json').write_text(json.dumps(self.schema),encoding='utf-8')

    def run_client(self, *args):
        env = dict(os.environ, PYTHONIOENCODING='cp1252')
        return subprocess.run([sys.executable,str(CLIENT),str(self.root),*args],env=env,
                              capture_output=True,timeout=15)

    def argument_file(self, value):
        path = self.root/'arguments.json'
        path.write_text(json.dumps(value),encoding='utf-16')
        return '@'+str(path)

    def server(self, status, body):
        requests = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self,*args): pass
            def do_POST(self):
                requests.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
                payload=json.dumps(body).encode('utf-8')
                self.send_response(status);self.send_header('Content-Length',str(len(payload)))
                self.end_headers();self.wfile.write(payload)
        server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        self.addCleanup(server.server_close);self.addCleanup(server.shutdown)
        (self.root/'connection.json').write_text(json.dumps({'url':f'http://127.0.0.1:{server.server_port}'}))
        return requests

    def test_unicode_discovery_under_legacy_windows_encoding(self):
        result=self.run_client('list')
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(json.loads(result.stdout)[0]['description'],'Unicode → 漢字')

    def test_powershell_extended_string_is_rejected_before_network(self):
        result=self.run_client('probe',self.argument_file({'story':{'value':'hello','PSPath':'fixture'}}))
        self.assertEqual(result.returncode,2)
        detail=json.loads(result.stdout)['error']
        self.assertIn('must be string',detail)
        self.assertIn('ReadAllText',detail)
        self.assertFalse((self.root/'connection.json').exists())

    def test_preserves_http_error_body(self):
        requests=self.server(500,{'error':{'code':-32602,'message':'invalid type: map, expected a string'}})
        result=self.run_client('probe',self.argument_file({'story':'hello'}))
        self.assertEqual(result.returncode,1,result.stderr)
        body=json.loads(result.stdout)
        self.assertEqual(body['http_status'],500)
        self.assertEqual(body['error']['error']['code'],-32602)
        self.assertIn('expected a string',body['error']['error']['message'])
        self.assertEqual(requests[0]['arguments'],{'story':'hello'})

    def test_valid_utf16_arguments_and_unicode_response_round_trip(self):
        requests=self.server(200,{'content':[{'type':'text','text':'✓ 保存'}]})
        result=self.run_client('probe',self.argument_file({'story':'Åä 漢字','context':None}))
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(json.loads(result.stdout)['content'][0]['text'],'✓ 保存')
        self.assertEqual(requests[0]['arguments'],{'story':'Åä 漢字','context':None})

    def test_mcp_error_is_not_success(self):
        self.server(200,{'isError':True,'content':[{'type':'text','text':'lookup failed'}]})
        result=self.run_client('probe',self.argument_file({'story':'hello'}))
        self.assertEqual(result.returncode,1)
        self.assertTrue(json.loads(result.stdout)['isError'])


if __name__=='__main__': unittest.main()
