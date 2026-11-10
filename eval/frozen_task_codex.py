"""Execute the existing frozen collection with isolated persistent Codex sessions.

No model selection, retries, response repair, tool use or evaluation criteria are
added here. Execution evidence is private; failed runs are retained, not resumed.
"""
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import threading
import time
import tomllib
import uuid

import frozen_task_run as collection
import frozen_task_eval as evaluation

FEATURES = ('shell_tool', 'apps', 'multi_agent', 'view_image', 'browser_use',
            'browser_use_external', 'computer_use', 'memories', 'daemon_auto_start',
            'hooks', 'plugins', 'plugin_discovery_prefetch', 'remote_plugin',
            'skill_search', 'skill_mcp_dependency_install', 'external_agent_memory_import')
SCHEMA = dict(type='object', additionalProperties=False,
              properties=dict(decision={'type': 'string'}, **{
                  key: dict(type='array', items={'type': 'string'})
                  for key in ('plan', 'evidence', 'uncertainties', 'read_requests')}),
              required=['decision', 'plan', 'evidence', 'uncertainties', 'read_requests'])
MAX_OUTPUT = 1024 * 1024


def _require(ok, message):
    if not ok:
        raise ValueError(message)


def _ref(path):
    raw = path.read_bytes()
    return dict(path=str(path), bytes=len(raw), sha256=collection.digest(raw))


def _configuration(home):
    path = home / 'config.toml'
    raw = path.read_bytes() if path.exists() else b''
    _require(len(raw) <= MAX_OUTPUT, 'User configuration exceeds bound')
    parsed = tomllib.loads(raw.decode('utf-8'))
    configured = [parsed]
    profile = parsed.get('profile')
    if isinstance(profile, str) and isinstance(parsed.get('profiles'), dict):
        configured.append(parsed['profiles'].get(profile, {}))
    _require(all(isinstance(config, dict) for config in configured), 'Invalid selected configuration profile')
    _require(all(not config.get(key) for config in configured for key in
                 ('developer_instructions', 'model_instructions_file', 'instructions_file', 'instructions')),
             'Configured instruction override is not permitted for frozen execution')
    effective = {}
    for config in configured:
        effective.update(config)
    fields = {key: effective[key] for key in ('model', 'model_provider', 'model_reasoning_effort',
              'model_reasoning_summary', 'profile') if key in effective}
    _require(all(isinstance(value, str) and re.fullmatch(r'[A-Za-z0-9_.:/-]{1,200}', value)
                 for value in fields.values()), 'Unsupported configured selection field')
    # Retain only a digest and selection fields, never configuration credentials.
    return dict(path=str(path), exists=path.exists(), sha256=collection.digest(raw), selection=fields)


def _process(argv, cwd, prompt, receipt, changed, timeout, event=None, evidence=None,
             response=None, stdout_limit=8*MAX_OUTPUT, stderr_limit=2*MAX_OUTPUT):
    """Observe one owned child; timeouts never resend a prompt or replace it."""
    messages = queue.Queue(maxsize=128)
    stopped = threading.Event()
    child = None
    threads = []
    streams = {}
    outputs = {'stdout': bytearray(), 'stderr': bytearray()}
    limits = {'stdout': stdout_limit, 'stderr': stderr_limit}
    stdin_result = {'complete': False}
    started = time.monotonic()
    receipt.update(argv=argv, invoked_at=collection.now(), status='starting', stdin_bytes=len(prompt))
    changed()

    def offer(value):
        while not stopped.is_set():
            try:
                messages.put(value, timeout=0.1)
                return True
            except queue.Full:
                pass
        return False

    def reader(name, stream):
        try:
            while chunk := stream.read1(8192):
                if not offer((name, chunk)):
                    break
        except BaseException:
            offer(('reader_error', name))
        finally:
            offer(('eof', name))

    def writer():
        try:
            count = child.stdin.write(prompt)
            child.stdin.close()
            stdin_result['complete'] = count == len(prompt)
        except BaseException:
            offer(('writer_error', None))

    try:
        if evidence:
            streams = {name: (evidence / filename).open('xb')
                       for name, filename in [('stdout', 'events.jsonl'), ('stderr', 'stderr.log')]}
        child = subprocess.Popen(argv, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, shell=False,
                                 creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0)
        receipt.update(pid=child.pid, status='running')
        changed()
        for name in ('stdout', 'stderr'):
            thread = threading.Thread(target=reader, args=(name, getattr(child, name)), daemon=True)
            thread.start(); threads.append(thread)
        thread = threading.Thread(target=writer, daemon=True)
        thread.start(); threads.append(thread)
        ended = set()
        pending = b''
        heartbeat = started
        while len(ended) < 2 or child.poll() is None:
            current = time.monotonic()
            if current - started > timeout:
                raise TimeoutError('Explicit Codex execution timeout; no retry')
            if current - heartbeat >= 25:
                print(json.dumps(dict(execution='observing_same_child', pid=child.pid,
                                      elapsed_seconds=round(current-started))), flush=True)
                heartbeat = current
            if response and response.exists():
                _require(response.stat().st_size <= MAX_OUTPUT, 'Response exceeds 1 MiB')
            try:
                name, chunk = messages.get(timeout=0.1)
            except queue.Empty:
                continue
            if name == 'eof':
                ended.add(chunk); continue
            _require(name in outputs, 'Codex pipe reader or stdin writer failed')
            remaining = limits[name] - len(outputs[name])
            retained = chunk[:remaining]
            outputs[name].extend(retained)
            if name in streams:
                streams[name].write(retained); streams[name].flush()
            if len(chunk) > remaining:
                receipt[name + '_truncated'] = True
                raise ValueError('Codex ' + name + ' exceeds output bound')
            if name == 'stdout' and event:
                pending += chunk
                while b'\n' in pending:
                    line, pending = pending.split(b'\n', 1)
                    _require(0 < len(line) <= MAX_OUTPUT, 'Invalid JSONL event length')
                    event(json.loads(line))
                _require(len(pending) <= MAX_OUTPUT, 'JSONL event exceeds bound')
        if event and pending.strip():
            event(json.loads(pending))
        receipt['exit_code'] = child.wait()
        _require(receipt['exit_code'] == 0, 'Codex child exited unsuccessfully')
        receipt['status'] = 'exited_zero'
        return bytes(outputs['stdout']), bytes(outputs['stderr'])
    except BaseException as error:
        receipt.update(status='failed', failure=type(error).__name__ + ': ' + str(error))
        if child is not None and child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                child.kill(); child.wait(timeout=10)
            receipt['terminated_owned_child'] = True
        raise
    finally:
        stopped.set()
        if child is not None:
            if child.poll() is None:
                child.terminate(); child.wait(timeout=10)
            receipt['exit_code'] = child.returncode
            for stream in (child.stdin, child.stdout, child.stderr):
                stream.close()
        for thread in threads:
            thread.join(timeout=10)
        # Preserve bytes already read before a rejected event/owned termination.
        while not messages.empty():
            name, chunk = messages.get_nowait()
            if name in outputs:
                remaining = limits[name] - len(outputs[name])
                outputs[name].extend(chunk[:remaining])
                if name in streams:
                    streams[name].write(chunk[:remaining])
                if len(chunk) > remaining:
                    receipt[name + '_truncated'] = True
        receipt['owned_threads_closed'] = all(not thread.is_alive() for thread in threads)
        receipt['stdin_complete'] = stdin_result['complete']
        for stream in streams.values():
            stream.close()
        receipt.update(finished_at=collection.now(), elapsed_seconds=time.monotonic()-started,
                       stdout_bytes=len(outputs['stdout']), stderr_bytes=len(outputs['stderr']))
        changed()


def _selection(state, selection):
    selected = {k: v for k, v in selection.items() if k in ('model', 'effort', 'model_provider') and v is not None}
    if not selected:
        return
    _require(all(isinstance(v, str) and len(v) <= 200 for v in selected.values()), 'Invalid observed model selection')
    configured = state['configuration']['selection']
    if 'model' in selected and 'model' in configured:
        _require(selected['model'] == configured['model'], 'Observed model differs from configured selection')
    if 'effort' in selected and 'model_reasoning_effort' in configured:
        _require(selected['effort'] == configured['model_reasoning_effort'], 'Observed reasoning differs from configured selection')
    observed = state.setdefault('observed_selection', {})
    for key, value in selected.items():
        _require(key not in observed or observed[key] == value, 'Codex model or reasoning selection changed')
        observed[key] = value


def _rollouts(home, session, state):
    found = []
    folder = home / 'sessions'
    if folder.exists():
        for path in folder.rglob('*' + session + '*.jsonl'):
            _require(path.resolve().is_relative_to(folder.resolve()) and not path.is_symlink(), 'Rollout escaped session directory')
            row = dict(path=str(path), retention='Persistent Codex session; retain for review, never removed by executor')
            if path.stat().st_size <= 16*MAX_OUTPUT:
                row.update(_ref(path))
                for line in path.read_bytes().splitlines():
                    value = json.loads(line)
                    if value.get('type') == 'turn_context':
                        _selection(state, value.get('payload', {}))
            else:
                row['contents_uninspected'] = 'Rollout exceeds bounded observation'
            found.append(row)
    return found


def _turn(root, work, prefix, overrides, schema, session, candidate, prompt_path,
          state, save, timeout, home):
    number = len(state['turns']) + 1
    directory = root / 'execution' / ('turn-' + str(number).zfill(3))
    directory.mkdir()
    prompt = prompt_path.read_bytes()
    _require(len(prompt) <= MAX_OUTPUT, 'Frozen packet exceeds bound')
    (directory / 'stdin.json').write_bytes(prompt)
    response = directory / 'response.json'
    turn = dict(number=number, role='candidate' if candidate else 'judge', candidate_id=candidate,
                prompt=_ref(prompt_path), stdin=_ref(directory / 'stdin.json'), requested_session=session)
    state['turns'].append(turn); save()
    argv = prefix + ['exec', '--sandbox', 'read-only', '--cd', str(work)] + overrides
    if session:
        argv += ['resume', session]
    argv += ['--json', '--skip-git-repo-check', '-o', str(response)]
    if candidate:
        argv += ['--output-schema', str(schema)]
    argv += ['-']
    observed = dict(thread=None, started=False, completed=False, final=None)

    def event(value):
        _require(isinstance(value, dict) and not observed['completed'], 'Unexpected event after completion')
        kind = value.get('type')
        if kind == 'thread.started':
            identifier = value.get('thread_id')
            _require(isinstance(identifier, str) and str(uuid.UUID(identifier)) == identifier,
                     'Invalid persistent thread identity')
            _require(observed['thread'] is None, 'Duplicate thread.started')
            if session:
                _require(identifier == session, 'Resumed the wrong persistent thread')
            else:
                _require(identifier not in state['session_ids'], 'Fresh agent reused a prior session')
                identity = 'codex:' + identifier
                if candidate:
                    collection.bind(root, candidate, identity)
                else:
                    current, _, _ = collection.load_state(root)
                    collection.identity_available(current, identity)
                    state['judge_agent'] = identity
                state['session_ids'].append(identifier)
            observed['thread'] = identifier
            turn.update(session_id=identifier, bound_at=collection.now())
            save()
        elif kind == 'turn.started':
            _require(observed['thread'] is not None and not observed['started'], 'Invalid turn.started')
            observed['started'] = True
            _selection(state, value)
        elif kind in ('item.started', 'item.updated', 'item.completed'):
            item = value.get('item', {})
            _require(item.get('type') != 'error', 'Codex emitted an error item; no response accepted')
            _require(observed['started'] and item.get('type') in ('reasoning', 'agent_message'),
                     'Tool activity or unknown item contaminates frozen evaluation')
            if kind == 'item.completed' and item['type'] == 'agent_message':
                _require(isinstance(item.get('text'), str), 'Agent message lacks text')
                observed['final'] = item['text']
        elif kind == 'turn.completed':
            _require(observed['started'] and observed['final'] is not None, 'Incomplete agent turn')
            usage = value.get('usage')
            _require(isinstance(usage, dict) and all(type(v) is int and v >= 0 for v in usage.values()), 'Invalid usage event')
            turn['usage'] = usage
            observed['completed'] = True
        else:
            raise ValueError('Unknown or failed Codex event; no retry')

    try:
        _require(_configuration(home) == state['configuration'], 'User configuration changed before invocation')
        _process(argv, work, prompt, turn, save, timeout, event, directory, response)
        _require(observed['completed'] and turn.get('owned_threads_closed') and turn.get('stdin_complete'),
                 'No completed isolated Codex turn with full stdin delivery')
        raw = collection.response_bytes(response)
        # CLI output-file transport can add or convert newlines, not content.
        normal = lambda text: text.replace('\r\n', '\n').rstrip('\n')
        _require(normal(raw.decode('utf-8')) == normal(observed['final']), 'Output file differs from final agent_message')
        _require(_configuration(home) == state['configuration'], 'User configuration changed during execution')
        turn['rollouts'] = _rollouts(home, observed['thread'], state)
        _require(prompt_path.read_bytes() == prompt, 'Frozen prompt changed during execution')
        identity = 'codex:' + observed['thread']
        if candidate:
            collection.record(root, candidate, identity, response)
        else:
            collection.judge(root, identity, response)
        turn['accepted_at'] = collection.now()
        return observed['thread']
    finally:
        for name in ('events.jsonl', 'stderr.log', 'response.json'):
            path = directory / name
            if path.exists() and path.stat().st_size <= 8*MAX_OUTPUT:
                turn[name] = _ref(path)
        save()


def execute(root: Path, executable: Path, timeout_seconds=900) -> dict:
    executable = executable.resolve(strict=True)
    _require(executable.is_file(), 'Codex executable must be a native file')
    if os.name == 'nt':
        with executable.open('rb') as stream:
            _require(executable.suffix.lower() == '.exe' and stream.read(2) == b'MZ', 'Native Windows Codex executable required')
    else:
        _require(os.access(executable, os.X_OK), 'Executable permission required')
    home = Path(os.environ.get('CODEX_HOME', str(Path.home() / '.codex'))).resolve()
    return _execute(root, [str(executable)], timeout_seconds, home)


def _execute(root, prefix, timeout_seconds, home):
    """Command-prefix injection is solely for owned fake-subprocess tests."""
    root = root.resolve(strict=True)
    _require(timeout_seconds > 0, 'Execution timeout must be positive')
    lock = root / '.codex-execution.lock'
    with lock.open('x', encoding='utf-8') as held:
        try:
            held.write(str(os.getpid())); held.flush()
            initial, _, _ = collection.load_state(root)
            _require(initial['status'] == 'awaiting_candidates' and len(initial['runs']) == 8 and
                     all('agent' not in run and 'response' not in run and len(run['turns']) == 1 and
                         run['delivery_mode'] == 'manual_packet_v1' for run in initial['runs']),
                     'Executor requires an untouched manual-packet collection')
            directory = root / 'execution'
            directory.mkdir()  # A failed/started executor cannot silently replay.
            work = directory / 'work'; work.mkdir()
            schema = directory / 'candidate-schema.json'; schema.write_bytes(collection.encoded(SCHEMA))
            state = dict(version=1, status='starting', created_at=collection.now(), configuration=None,
                         producer_kind=initial.get('producer_kind', 'historical'),
                         adapter_sha256=collection.digest(Path(__file__).read_bytes()), session_ids=[], turns=[],
                         timeout_seconds_per_invocation=timeout_seconds,
                         retention='Retain private execution artifacts and persistent Codex sessions for review; no automatic removal.',
                         limits=('Frozen current-producer task run; no fresh-holdout or comparative implementation/QA claim.'
                                 if initial.get('producer_kind') == 'current_producer' else
                                 'Frozen historical task run; no current-tool or comparative implementation/QA claim.'))

            def save():
                stage = directory / '.state.writing'
                stage.write_bytes(collection.encoded(state))
                os.replace(stage, directory / 'state.json')

            save()
            try:
                state['configuration'] = _configuration(home); save()
                # Discovery output can include MCP credentials. It stays in memory;
                # only names are retained and no failure prints the output.
                discovery = {}
                raw, _ = _process(prefix + ['mcp', 'list', '--json'], work, b'', discovery,
                                  lambda: None, min(timeout_seconds, 30), stdout_limit=MAX_OUTPUT)
                servers = json.loads(raw)
                _require(isinstance(servers, list), 'MCP discovery must return a list')
                names = [row['name'] for row in servers]
                _require(all(isinstance(name, str) and re.fullmatch(r'[A-Za-z0-9_-]{1,256}', name)
                             for name in names), 'MCP name cannot be disabled with an unambiguous CLI key')
                _require(len(set(names)) == len(names), 'Duplicate MCP name')
                state['disabled_mcp_names'] = names
                version = {}
                raw, _ = _process(prefix + ['--version'], work, b'', version,
                                  lambda: None, min(timeout_seconds, 30), stdout_limit=4096)
                _require(re.fullmatch(rb'codex-cli [0-9][A-Za-z0-9.+-]*\s*', raw), 'Unrecognized Codex version')
                state['codex_version'] = raw.decode().strip()
                overrides = []
                values = ['approval_policy="never"', 'project_doc_max_bytes=0', 'web_search="disabled"',
                          'suppress_unstable_features_warning=true']
                values += ['features.' + name + '=false' for name in FEATURES]
                values += ['features.skip_host_skill_discovery=true', 'features.omit_skill_descriptions=true']
                values += ['mcp_servers.' + name + '.enabled=false' for name in names]
                for value in values:
                    overrides += ['-c', value]
                state['status'] = 'running'; save()
                for run in initial['runs']:
                    session = None
                    while True:
                        _require(_configuration(home) == state['configuration'], 'User configuration changed during execution')
                        current, _, _ = collection.load_state(root)
                        active = next(r for r in current['runs'] if r['opaque_id'] == run['opaque_id'])
                        if 'response' in active:
                            break
                        packet = evaluation.bounded(root, active['turns'][-1]['prompt'])
                        session = _turn(root, work, prefix, overrides, schema, session, active['opaque_id'],
                                        packet, state, save, timeout_seconds, home)
                _require(len(state['session_ids']) == 8, 'Expected eight distinct candidate sessions')
                current, _, _ = collection.load_state(root)
                _require(current['status'] == 'awaiting_judge', 'Candidate collection did not finish')
                _turn(root, work, prefix, overrides, schema, None, None,
                      evaluation.bounded(root, 'judge/prompt.json'), state, save, timeout_seconds, home)
                _require(len(state['session_ids']) == 9, 'Judge must use a separate fresh session')
                state.update(status='complete', finished_at=collection.now()); save()
                return dict(collection.status(root), execution=str(directory / 'state.json'))
            except BaseException as error:
                state.update(status='failed', finished_at=collection.now(),
                             failure=type(error).__name__ + ': ' + str(error))
                save()
                raise
        finally:
            held.close()
            lock.unlink()
