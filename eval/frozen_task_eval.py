"""Inspect frozen task evaluations without calling a model or indexing a project.

Private source packets stay in the operator's recorded corpus directory. Fresh
candidate/judge agents run separately; verifying saved outputs is not a new run.
"""
import argparse
import datetime
import hashlib
import json
from pathlib import Path, PurePosixPath
import sys

CATEGORIES = {'scoped_policy', 'companion_edits', 'history', 'diagnosis'}
INSTRUCTIONS = (
    'Work only from this task and supplied evidence. Do not use tools, inspect '
    'files, contact services, or change anything. You may request up to three '
    'IDs from available_reads; the coordinator will supply their frozen bytes. '
    'Do not infer accepted policy from implementation alone. Return JSON only: '
    '{"decision":string,"plan":[string],"evidence":[string],'
    '"uncertainties":[string],"read_requests":[string]}. '
    'If evidence is insufficient, state exactly what is missing. Do not mention '
    'an experiment arm, system origin or treatment label; cite source paths.'
)


def bounded(root, relative):
    part = PurePosixPath(relative)
    if part.is_absolute() or '..' in part.parts or '\\' in relative or ':' in relative:
        raise ValueError('Use a normalized corpus-relative path')
    path = (root / relative).resolve(strict=True)
    if not path.is_relative_to(root) or not path.is_file() or path.stat().st_size > 1024 * 1024:
        raise ValueError('Corpus input escaped its root or exceeds 1 MiB')
    return path


def read_json(root, relative):
    return json.loads(bounded(root, relative).read_text(encoding='utf-8'))


def candidate_response(root, relative, canary):
    raw = bounded(root, relative).read_bytes()
    return raw, parse_candidate_response(raw, canary)


def parse_candidate_response(raw, canary):
    if canary in raw:
        raise ValueError('Judge-only canary appeared in a candidate response')
    parsed = json.loads(raw)
    if canary in json.dumps(parsed, ensure_ascii=False).encode('utf-8'):
        raise ValueError('Judge-only canary appeared in a candidate response')
    if (not isinstance(parsed, dict)
            or set(parsed) != {'decision','plan','evidence','uncertainties','read_requests'}
            or not isinstance(parsed['decision'], str)
            or any(not isinstance(parsed[key], list)
                   or any(not isinstance(item, str) for item in parsed[key])
                   for key in ('plan','evidence','uncertainties','read_requests'))):
        raise ValueError('Candidate did not return the required response shape')
    return parsed


def delivery_instruction(root, relative):
    path = bounded(root, relative)
    return (
        f'To receive the frozen task packet, read ONLY this UTF-8 file: {path}\n'
        'This read-only packet delivery is the only authorized tool use. '
        'If needed, read consecutive chunks of at most 12000 characters until '
        'the complete JSON has been received; never rely on truncated output. '
        'Do not inspect other paths, contact services, or change files. '
        'After receiving the packet, follow its task and instructions without '
        'further tool use. Return its requested JSON only. Request additional '
        'evidence only through its available_reads IDs; the coordinator will '
        'deliver another frozen packet if needed.'
    )


def load(root):
    anchor = read_json(root, 'freeze_record.json')
    if hashlib.sha256(bounded(root,'manifest.json').read_bytes()).hexdigest() != anchor['manifest_sha256']:
        raise ValueError('Frozen task manifest changed')
    manifest = read_json(root, 'manifest.json')
    for relative, digest in manifest['frozen_files'].items():
        if hashlib.sha256(bounded(root, relative).read_bytes()).hexdigest() != digest:
            raise ValueError('Frozen input changed: ' + relative)
    cases = manifest['cases']
    if {case['category'] for case in cases} != CATEGORIES or len(cases) != 4:
        raise ValueError('Pilot requires four distinct frozen task categories')
    if len({case['id'] for case in cases}) != 4:
        raise ValueError('Task IDs must be unique')
    oracle = read_json(root, manifest['judge_oracle'])
    referenced = {manifest['judge_oracle']}
    for case in cases:
        referenced.update(arm['input'] for arm in case['arms'].values())
        referenced.update(case['available_reads'].values())
    if not referenced <= set(manifest['frozen_files']):
        raise ValueError('Every task input, readable source and oracle must be frozen')
    canary = oracle['judge_only_canary'].encode('utf-8')
    for case in cases:
        if set(case['arms']) != {'baseline', 'assisted'}:
            raise ValueError('Each task needs both frozen arms')
        for arm in case['arms'].values():
            if canary in json.dumps(read_json(root, arm['input']), ensure_ascii=False).encode('utf-8'):
                raise ValueError('Judge-only evidence leaked into a candidate input')
        for source in case['available_reads'].values():
            if canary in json.dumps(read_json(root, source), ensure_ascii=False).encode('utf-8'):
                raise ValueError('Judge-only evidence leaked into a readable source')
    return manifest, oracle


def packet(root, manifest, case_id, arm, requested):
    case = next(case for case in manifest['cases'] if case['id'] == case_id)
    if len(requested) > 3 or len(set(requested)) != len(requested):
        raise ValueError('Use at most three distinct read requests per turn')
    if requested:
        return dict(case_id=case_id, instructions='Use these frozen read results to finish the prior task; return the same JSON shape.',
            read_results={key: read_json(root, case['available_reads'][key]) for key in requested})
    return dict(case_id=case_id, instructions=INSTRUCTIONS,
        task=case['task'], source_inputs=read_json(root, case['arms'][arm]['input']),
        available_reads=sorted(case['available_reads']))


def results(root, manifest, oracle, judgments=None):
    runs = read_json(root, 'runs.json')
    if judgments is None:
        judgments = read_json(root, 'judgments.json')
    if not isinstance(judgments, list) or any(not isinstance(j, dict) for j in judgments):
        raise ValueError('Judgments must be a list of candidate judgments')
    expected = {(case['id'], arm) for case in manifest['cases'] for arm in case['arms']}
    if {(run['case_id'], run['arm']) for run in runs} != expected or len(runs) != 8:
        raise ValueError('Results require exactly eight candidate runs')
    modes = {run.get('delivery_mode', 'manual_packet_v1') for run in runs}
    if len(modes) != 1 or not modes <= {'manual_packet_v1', 'readonly_packet_v2'}:
        raise ValueError('Paired runs require one consistent recorded delivery protocol')
    if modes == {'readonly_packet_v2'}:
        protocol_bytes = bounded(root, 'delivery-protocol-v2.json').read_bytes()
        anchor = bounded(root, 'delivery-protocol-v2.sha256').read_text(encoding='utf-8').strip()
        if hashlib.sha256(protocol_bytes).hexdigest() != anchor:
            raise ValueError('Packet delivery protocol changed after freezing')
        protocol = json.loads(protocol_bytes)
        if protocol['source_manifest_sha256'] != hashlib.sha256(bounded(root, 'manifest.json').read_bytes()).hexdigest():
            raise ValueError('Delivery protocol points to a different frozen corpus')
    canary = oracle['judge_only_canary'].encode('utf-8')
    metrics = []
    for run in runs:
        case = next(case for case in manifest['cases'] if case['id'] == run['case_id'])
        delivered = []
        read_count = 0
        intermediates = run.get('intermediate_responses', [])
        if len(intermediates) != max(0, len(run['turns']) - 1):
            raise ValueError('Every requested-read turn requires its captured model response')
        for number, turn in enumerate(run['turns']):
            if number:
                _, previous = candidate_response(root, intermediates[number - 1], canary)
                if previous['read_requests'] != turn['read_ids']:
                    raise ValueError('Delivered evidence differs from what the candidate requested')
            actual = bounded(root, turn['prompt']).read_bytes()
            expected_packet = packet(root, manifest, run['case_id'], run['arm'], turn['read_ids'])
            if json.loads(actual) != expected_packet or canary in actual:
                raise ValueError('Candidate input is not its frozen allowlisted packet')
            if run.get('delivery_mode') == 'readonly_packet_v2':
                authorization = turn['delivery_instruction']
                if authorization != delivery_instruction(root, turn['prompt']):
                    raise ValueError('Packet delivery authorization changed')
                delivered.append(authorization.encode('utf-8'))
            delivered.append(actual)
            read_count += len(turn['read_ids'])
        if read_count > 8 or not run['turns'] or run['turns'][0]['read_ids']:
            raise ValueError('Candidate read budget or initial packet is invalid')
        response, parsed = candidate_response(root, run['response'], canary)
        if parsed['read_requests']:
            raise ValueError('Candidate ended with unresolved read requests')
        elapsed = (datetime.datetime.fromisoformat(run['finished_at']) - datetime.datetime.fromisoformat(run['started_at'])).total_seconds()
        if elapsed < 0:
            raise ValueError('Invalid candidate wall time')
        metrics.append(dict(case_id=run['case_id'],arm=run['arm'],
            input_bytes=sum(map(len,delivered)),input_characters=sum(len(x.decode('utf-8')) for x in delivered),
            output_bytes=len(response),output_characters=len(response.decode('utf-8')),
            additional_reads=read_count,observed_agent_wall_seconds=elapsed))
    # Judge labels are opaque. Their hidden mapping is kept out of the judge
    # input and joined only after the independent judgment has been captured.
    mapping = read_json(root, 'judge/mapping.json')
    if {(m['case_id'],m['arm']) for m in mapping.values()} != expected or len(mapping) != 8:
        raise ValueError('Opaque judge mapping does not cover all eight runs')
    if len(judgments) != 8 or {j['candidate_id'] for j in judgments} != set(mapping):
        raise ValueError('Independent judgments are incomplete')
    prompt = read_json(root,'judge/prompt.json')
    if set(prompt) != {'instructions','tasks','candidates','oracle'}:
        raise ValueError('Unexpected judge input or exposed run metadata')
    if prompt['oracle'] != oracle or prompt['tasks'] != {
            case['id']: case['task'] for case in manifest['cases']}:
        raise ValueError('Judge task or oracle differs from frozen evidence')
    if {c['candidate_id'] for c in prompt['candidates']} != set(mapping):
        raise ValueError('Judge did not receive all opaque candidates')
    for candidate in prompt['candidates']:
        if set(candidate) != {'candidate_id','case_id','response'}:
            raise ValueError('Candidate arm or metrics exposed to judge')
        identity = mapping[candidate['candidate_id']]
        run = next(r for r in runs if (r['case_id'],r['arm']) == (identity['case_id'],identity['arm']))
        if candidate['case_id'] != identity['case_id'] or candidate['response'] != read_json(root,run['response']):
            raise ValueError('Judge candidate differs from the captured response')
    for judgment in judgments:
        if (not isinstance(judgment.get('false_guidance'), list)
                or any(not isinstance(item, str) for item in judgment['false_guidance'])):
            raise ValueError('False guidance must be a list of specific findings')
        identity = mapping[judgment['candidate_id']]
        item = next(m for m in metrics if (m['case_id'],m['arm']) == (identity['case_id'],identity['arm']))
        criteria = oracle['cases'][identity['case_id']]['criteria']
        if set(judgment['criteria']) != set(criteria):
            raise ValueError('Judgment omitted a frozen criterion')
        values = list(judgment['criteria'].values())
        if any(value not in ('satisfied','not_satisfied','justified_withholding') for value in values):
            raise ValueError('Unknown judgment value')
        item.update(satisfied=values.count('satisfied'),
            justified_withholding=values.count('justified_withholding'),
            total_criteria=len(values),false_guidance=len(judgment['false_guidance']))
    return metrics


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['verify','packet','report'])
    parser.add_argument('corpus',type=Path)
    parser.add_argument('--case')
    parser.add_argument('--arm',choices=['baseline','assisted'])
    parser.add_argument('--read',action='append',default=[])
    parser.add_argument('--require-results',action='store_true')
    args = parser.parse_args()
    root = args.corpus.resolve(strict=True)
    manifest, oracle = load(root)
    if args.command == 'packet':
        if not args.case or not args.arm:
            parser.error('packet needs --case and --arm')
        print(json.dumps(packet(root,manifest,args.case,args.arm,args.read),ensure_ascii=False,indent=2))
    elif args.command == 'report' or args.require_results:
        print(json.dumps(dict(status='saved_run_verified',metrics=results(root,manifest,oracle),
            limits='Saved-run integrity and recorded judgments, not a fresh model evaluation or completed OX implementation/QA productivity.'),indent=2))
    else:
        print(json.dumps(dict(status='frozen_inputs_verified',cases=4,candidate_runs_not_verified=True)))


if __name__ == '__main__':
    try:
        if hasattr(sys.stdout,'reconfigure'):
            sys.stdout.reconfigure(encoding='utf-8')
        main()
    except (ValueError,KeyError,FileNotFoundError,StopIteration) as error:
        raise SystemExit(str(error))
