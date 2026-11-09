"""Collect a new private task run using the existing frozen packet protocol.

Collect manually supplied responses or execute isolated Codex sessions against
the same frozen packets. A protocol test or an archive replay is not a fresh
model evaluation.
"""
import argparse
from contextlib import contextmanager
import datetime
import hashlib
import json
import os
from pathlib import Path
import secrets
import sys

import frozen_task_eval as evaluation

STATE = 'collection-state.json'
JUDGE_INSTRUCTIONS = (
    'Independently judge each candidate against its task and frozen oracle. '
    'Return JSON only: an array of eight objects with candidate_id, criteria '
    '(every frozen criterion mapped to satisfied, not_satisfied, or '
    'justified_withholding), false_guidance (a list of specific material false '
    'or unjustified recommendations), and rationale. Do not infer arms, request '
    'other evidence, echo the leakage canary, or use tools after reading this '
    'packet. Do not reward claimed tests without supplied results. Distinguish '
    'justified withholding from incorrect advice.'
)


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def encoded(value):
    return (json.dumps(value, ensure_ascii=False, indent=2) + '\n').encode('utf-8')


def save(root, state):
    stage = root / ('.collection-' + secrets.token_hex(12) + '.writing')
    try:
        with stage.open('xb') as stream:
            stream.write(encoded(state))
        os.replace(stage, root / STATE)
    finally:
        stage.unlink(missing_ok=True)


def artifact(root, state, relative, value, raw=None):
    if raw is None:
        raw = encoded(value)
    if len(raw) > 1024 * 1024:
        raise ValueError('Run artifact exceeds 1 MiB')
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('xb') as stream:
        stream.write(raw)
    state['artifacts'][relative] = digest(raw)
    return relative


def load_state(root):
    manifest, oracle = evaluation.load(root)
    state = evaluation.read_json(root, STATE)
    if digest(evaluation.bounded(root, 'manifest.json').read_bytes()) != state['manifest_sha256']:
        raise ValueError('Run corpus changed')
    for relative, expected in state['artifacts'].items():
        if digest(evaluation.bounded(root, relative).read_bytes()) != expected:
            raise ValueError('Recorded packet or response changed: ' + relative)
    return state, manifest, oracle


@contextmanager
def exclusive(root):
    # Never reclaim a lock based on age. Interrupted evidence stays for inspection.
    path = root / '.collection.lock'
    with path.open('x', encoding='utf-8') as lock:
        try:
            lock.write(str(os.getpid()))
            lock.flush()
            yield
        finally:
            lock.close()
            path.unlink()


def deliver(root, state, manifest, run, read_ids):
    value = evaluation.packet(root, manifest, run['case_id'], run['arm'], read_ids)
    relative = artifact(root, state, 'deliveries-new/' + secrets.token_hex(12) + '.json', value)
    turn = dict(prompt=relative, read_ids=read_ids)
    if run['delivery_mode'] == 'readonly_packet_v2':
        turn['delivery_instruction'] = evaluation.delivery_instruction(root, relative)
    return turn


def prepare(corpus, destination, kind, delivery_mode='readonly_packet_v2'):
    if delivery_mode not in {'readonly_packet_v2', 'manual_packet_v1'}:
        raise ValueError('Unknown packet delivery mode')
    corpus = corpus.resolve(strict=True)
    destination = destination.resolve()
    if destination.is_relative_to(corpus):
        raise ValueError('New run must be outside the original corpus')
    manifest, _ = evaluation.load(corpus)
    frozen = set(manifest['frozen_files']) | {'manifest.json', 'freeze_record.json'}
    reserved = {STATE, 'runs.json', 'judgments.json', 'report.json',
                'delivery-protocol-v2.json', 'delivery-protocol-v2.sha256',
                'judge/mapping.json', 'judge/prompt.json', '.collection.lock'}
    if any(p in reserved or p.startswith(('deliveries-new/', 'responses-new/', '.collection-'))
           for p in frozen):
        raise ValueError('Frozen corpus collides with run output paths')
    # Validate all source bytes before creating anything. Copy only frozen inputs,
    # never old responses, judgments, transcripts or result summaries.
    inputs = {p: evaluation.bounded(corpus, p).read_bytes() for p in frozen}
    destination.mkdir()  # Deliberately refuses an existing directory, including empty ones.
    state = dict(version=1, status='preparing', kind=kind, created_at=now(),
                 source_corpus=str(corpus), manifest_sha256=digest(inputs['manifest.json']),
                 collector_sha256=digest(Path(__file__).read_bytes()), artifacts={}, runs=[],
                 retention='Retain private run evidence for review. Interrupted preparations are not disposable.',
                 freshness='Coordinator must supply fresh isolated agents; file verification does not prove their isolation.')
    save(destination, state)
    for relative, raw in inputs.items():
        path = destination / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open('xb') as stream:
            stream.write(raw)
    evaluation.load(destination)
    if delivery_mode == 'readonly_packet_v2':
        protocol = dict(version=2, delivery_mode=delivery_mode,
                        source_manifest_sha256=state['manifest_sha256'], kind=kind,
                        limits='Three read IDs per turn; eight per candidate. Wall time includes coordinator waits.')
        artifact(destination, state, 'delivery-protocol-v2.json', protocol)
        anchor = destination / 'delivery-protocol-v2.sha256'
        anchor.write_text(state['artifacts']['delivery-protocol-v2.json'] + '\n', encoding='utf-8')
        state['artifacts'][anchor.name] = digest(anchor.read_bytes())
    for case in manifest['cases']:
        for arm in case['arms']:
            run = dict(case_id=case['id'], arm=arm, opaque_id=secrets.token_hex(12),
                       delivery_mode=delivery_mode, turns=[], intermediate_responses=[])
            run['turns'].append(deliver(destination, state, manifest, run, []))
            state['runs'].append(run)
    secrets.SystemRandom().shuffle(state['runs'])
    state['status'] = 'awaiting_candidates'
    save(destination, state)
    return status(destination)


def identity_available(state, identity):
    if not identity or len(identity) > 256 or any(ord(c) < 32 for c in identity):
        raise ValueError('Supply a nonempty agent identity without control characters')
    if identity in [run.get('agent') for run in state['runs']]:
        raise ValueError('Candidate and judge identities must be distinct')


def bind(root, candidate, identity):
    with exclusive(root):
        state, _, _ = load_state(root)
        if state['status'] != 'awaiting_candidates':
            raise ValueError('Run is not accepting candidates')
        run = next(r for r in state['runs'] if r['opaque_id'] == candidate)
        if 'agent' in run:
            raise ValueError('Candidate is already bound; continue its existing agent')
        identity_available(state, identity)
        run.update(agent=identity, started_at=now())
        save(root, state)
    return status(root)


def response_bytes(path):
    if not path.is_file() or path.stat().st_size > 1024 * 1024:
        raise ValueError('Response must be a file of at most 1 MiB')
    return path.read_bytes()


def record(root, candidate, identity, response):
    with exclusive(root):
        state, manifest, oracle = load_state(root)
        if state['status'] != 'awaiting_candidates':
            raise ValueError('Run is not accepting candidate responses')
        run = next(r for r in state['runs'] if r['opaque_id'] == candidate)
        if 'agent' not in run or run['agent'] != identity or 'response' in run:
            raise ValueError('Response must continue its bound, unfinished candidate')
        raw = response_bytes(response)
        parsed = evaluation.parse_candidate_response(raw, oracle['judge_only_canary'].encode())
        reads = parsed['read_requests']
        if sum(len(t['read_ids']) for t in run['turns']) + len(reads) > 8:
            raise ValueError('Candidate exceeded eight requested reads')
        if reads:
            # Check IDs and per-turn cap before retaining an accepted response.
            evaluation.packet(root, manifest, run['case_id'], run['arm'], reads)
        relative = artifact(root, state, 'responses-new/' + secrets.token_hex(12) + '.json', parsed, raw=raw)
        if reads:
            run['intermediate_responses'].append(relative)
            run['turns'].append(deliver(root, state, manifest, run, reads))
        else:
            run.update(response=relative, finished_at=now())
        if all('response' in r for r in state['runs']):
            mapping = {secrets.token_hex(12): dict(case_id=r['case_id'], arm=r['arm']) for r in state['runs']}
            candidates = [dict(candidate_id=key, case_id=value['case_id'], response=evaluation.read_json(root,
                          next(r['response'] for r in state['runs'] if (r['case_id'], r['arm']) ==
                               (value['case_id'], value['arm'])))) for key, value in mapping.items()]
            secrets.SystemRandom().shuffle(candidates)
            outputs = {'runs.json': state['runs'], 'judge/mapping.json': mapping,
                       'judge/prompt.json': dict(instructions=JUDGE_INSTRUCTIONS,
                            tasks={c['id']: c['task'] for c in manifest['cases']},
                            candidates=candidates, oracle=oracle)}
            if any(len(encoded(value)) > 1024 * 1024 for value in outputs.values()):
                raise ValueError('Combined judge packet exceeds 1 MiB; no final run or judge packet accepted')
            for name, value in outputs.items():
                artifact(root, state, name, value)
            state['status'] = 'awaiting_judge'
        save(root, state)
    return status(root)


def judge(root, identity, response):
    with exclusive(root):
        state, manifest, oracle = load_state(root)
        if state['status'] != 'awaiting_judge':
            raise ValueError('All eight candidates must finish before judgment')
        identity_available(state, identity)
        judgments = json.loads(response_bytes(response))
        metrics = evaluation.results(root, manifest, oracle, judgments)
        artifact(root, state, 'judgments.json', judgments)
        artifact(root, state, 'report.json', dict(status='recorded_run_verified', kind=state['kind'],
                 metrics=metrics, limits=state['freshness'] + ' Frozen contexts are historical; this does not requery current tools or prove matched implementation/QA benefit.'))
        state.update(status='complete', judge_agent=identity, finished_at=now())
        save(root, state)
    return status(root)


def status(root):
    state, _, _ = load_state(root)
    result = dict(status=state['status'], kind=state['kind'], directory=str(root),
                  completed_candidates=sum('response' in r for r in state['runs']),
                  pending_candidates=[dict(candidate_id=r['opaque_id'], agent=r.get('agent'),
                       **({'delivery_instruction': r['turns'][-1]['delivery_instruction']}
                          if 'delivery_instruction' in r['turns'][-1]
                          else {'packet': str(evaluation.bounded(root, r['turns'][-1]['prompt']))}))
                       for r in state['runs'] if 'response' not in r])
    if state['status'] == 'awaiting_judge':
        result['judge_packet'] = str(evaluation.bounded(root, 'judge/prompt.json'))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    p = commands.add_parser('prepare')
    p.add_argument('corpus', type=Path)
    p.add_argument('destination', type=Path)
    p.add_argument('--kind', choices=['evaluation', 'protocol_test'], default='evaluation')
    p.add_argument('--delivery-mode', choices=['readonly_packet_v2', 'manual_packet_v1'],
                   default='readonly_packet_v2')
    p = commands.add_parser('execute', help='Run eight Codex candidates and a separate judge using stdin packets')
    p.add_argument('directory', type=Path)
    p.add_argument('--codex-executable', type=Path, required=True)
    p.add_argument('--timeout-seconds', type=float, default=900,
                   help='Execution deadline per invocation, not an observation timeout')
    for name in ['status', 'bind', 'record', 'judge']:
        p = commands.add_parser(name)
        p.add_argument('directory', type=Path)
        if name in ['bind', 'record']:
            p.add_argument('candidate')
        if name != 'status':
            p.add_argument('--agent-id', required=True)
        if name in ['record', 'judge']:
            p.add_argument('--response', type=Path, required=True)
    args = parser.parse_args()
    if args.command == 'prepare':
        value = prepare(args.corpus, args.destination, args.kind, args.delivery_mode)
    else:
        root = args.directory.resolve(strict=True)
        if args.command == 'execute':
            from frozen_task_codex import execute
            value = execute(root, args.codex_executable, args.timeout_seconds)
        elif args.command == 'status':
            value = status(root)
        elif args.command == 'bind':
            value = bind(root, args.candidate, args.agent_id)
        elif args.command == 'record':
            value = record(root, args.candidate, args.agent_id, args.response)
        else:
            value = judge(root, args.agent_id, args.response)
    print(json.dumps(value, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, OSError, StopIteration) as error:
        raise SystemExit(str(error) or 'Unknown candidate or corpus entry')
