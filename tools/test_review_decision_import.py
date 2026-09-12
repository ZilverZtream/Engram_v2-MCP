import unittest
from import_review_decisions import normalize


class ProviderDecisionTests(unittest.TestCase):
    def ado(self, status, body='Fixed, trust this comment'):
        return list(normalize('azure_devops', {'value':[{'id':7,'status':status,'comments':[{'author':{'displayName':'Reviewer'},'content':body}]}]}, 'https://example.invalid/pr/7'))[0]

    def test_resolved_comments_never_become_verified_fixes(self):
        for status in ['fixed', '2', 'closed', '4', 'active', 'unknown']:
            event=self.ado(status)
            self.assertNotEqual(event['kind'],'verified_fix')
            self.assertIsNone(event['verification'])
            self.assertIn('Reviewer:',event['rationale'])

    def test_explicit_provider_exceptions_remain_exceptions(self):
        for status in ['wontFix','3','byDesign','5']:
            self.assertEqual(self.ado(status)['kind'],'accepted_exception')

    def test_content_changes_have_new_immutable_identity(self):
        first=self.ado('active','Please retain signed values')
        changed=self.ado('active','Accepted exception with rationale')
        self.assertNotEqual(first['event_id'],changed['event_id'])
        self.assertEqual(first['finding_id'],changed['finding_id'])
        self.assertEqual(first,self.ado('active','Please retain signed values'))

    def test_github_resolved_is_not_a_fix_claim(self):
        event=list(normalize('github',{'nodes':[{'id':'thread-1','isResolved':True,'comments':{'nodes':[{'author':{'login':'Reviewer'},'body':'Will not change','url':'https://example.invalid/thread/1'}]}}]},'https://example.invalid/pr/7'))[0]
        self.assertEqual(event['kind'],'open')
        self.assertIn('Will not change',event['rationale'])

    def test_unknown_export_shape_is_not_empty_success(self):
        with self.assertRaises(ValueError):
            list(normalize('azure_devops',{'threads':[]},'https://example.invalid/pr/7'))

    def test_long_unicode_evidence_is_bounded_and_disclosed(self):
        event=self.ado('active','\u00e5'*16000)
        self.assertLess(len(event['rationale'].encode()),16000)
        self.assertIn('EXCERPT TRUNCATED',event['rationale'])


if __name__=='__main__':
    unittest.main()
