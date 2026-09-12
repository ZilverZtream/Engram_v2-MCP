"""Import saved Azure DevOps threads or GitHub reviewThreads into Engram.

Only reads an explicitly supplied JSON export. No network fetches, credentials,
comment posting, or interpretation of comment text as instructions. Provider
resolution metadata can establish a claim/exception, never a verified fix.
"""
import argparse
import hashlib
import json
from pathlib import Path
from evaluate_evidence_reliability import Client


def normalize(provider, data, review_url):
    if not review_url.startswith('https://'):
        raise ValueError('review URL must use https')
    field = 'value' if provider == 'azure_devops' else 'nodes'
    if not isinstance(data, dict) or not isinstance(data.get(field), list):
        raise ValueError(f'expected an exported thread collection with {field} array')
    threads = data[field]
    for thread in threads:
        tid = str(thread.get('id', ''))
        if not tid:
            raise ValueError('thread ID missing')
        comments = thread.get('comments', [])
        if isinstance(comments, dict):
            comments = comments.get('nodes', [])
        if not comments:
            continue
        if provider == 'azure_devops':
            raw_status = str(thread.get('status', 'unknown')).lower()
            # Azure enum: active=1, fixed=2, wontFix=3, closed=4, byDesign=5.
            status = {'1':'active','2':'fixed','3':'wontfix','4':'closed','5':'bydesign'}.get(raw_status,raw_status)
            kind = 'accepted_exception' if status in {'wontfix','bydesign'} else 'claimed_fix' if status == 'fixed' else 'open'
            source = review_url + ('&' if '?' in review_url else '?')+'discussionId='+tid
        else:
            status = 'resolved' if thread.get('isResolved') else 'open'
            # Resolving a GitHub conversation is not an assertion that code was
            # fixed. Keep it open for reconciliation unless an explicit decision
            # is subsequently recorded with its source and rationale.
            kind = 'open'
            source = comments[0].get('url') or review_url
        excerpts = []
        for comment in comments:
            author = comment.get('author') or {}
            actor = author.get('displayName') or author.get('login') or 'unknown author'
            body = comment.get('content', comment.get('body',''))
            excerpts.append(f'{actor}: {body}')
        rationale = f'Provider thread status: {status}. Resolution is not independently verified.\n'+'\n\n'.join(excerpts)
        if len(rationale.encode('utf-8')) > 14000:
            rationale = rationale.encode('utf-8')[:14000].decode('utf-8',errors='ignore')+'\n[EXCERPT TRUNCATED; consult source URL]'
        date = thread.get('lastUpdatedDate') or comments[-1].get('updatedAt') or comments[-1].get('lastUpdatedDate') or 'source timestamp unavailable'
        digest = hashlib.sha256(json.dumps(thread,sort_keys=True,ensure_ascii=False).encode()).hexdigest()
        yield {'event_id':f'{provider}:{tid}:{digest}','finding_id':f'{provider}:{tid}',
            'supersedes':None,'kind':kind,'source_url':source,
            'author':'provider thread metadata; disposition author unverified',
            'recorded_at':date,'rationale':rationale,'verification':None}


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--provider',choices=['azure_devops','github'],required=True)
    p.add_argument('--input',type=Path,required=True)
    p.add_argument('--review-url',required=True)
    p.add_argument('--review-id',required=True)
    p.add_argument('--project-id',required=True)
    p.add_argument('--binary',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True)
    p.add_argument('--config',type=Path)
    args=p.parse_args();args.output.mkdir(parents=True,exist_ok=True)
    events=list(normalize(args.provider,json.loads(args.input.read_text(encoding='utf-8-sig')),args.review_url))
    with Client(args.binary,args.output,args.config) as client:
        request={'project_id':args.project_id,'review_id':args.review_id}
        existing=json.loads(client.call('get_review_decisions',request))
        ids={e['event_id'] for e in existing['events']}
        latest={e['finding_id']:e['event_id'] for e in existing['events']}
        additions=[]
        for event in events:
            if event['event_id'] in ids:
                continue
            event['supersedes']=latest.get(event['finding_id'])
            latest[event['finding_id']]=event['event_id']
            additions.append(event)
        if additions:
            print(client.call('record_review_decisions',{**request,'decisions':additions}))
        else:
            print('No new review events.')
        (args.output/'import-manifest.json').write_text(json.dumps({'input_sha256':hashlib.sha256(args.input.read_bytes()).hexdigest(),'provider':args.provider,'review_id':args.review_id,'events_added':len(additions)},indent=2))


if __name__=='__main__':
    main()
