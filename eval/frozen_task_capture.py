"""Freeze current installed producer replies into the existing four-task corpus.

Private, Windows, existing-daemon only. Never starts a server, opens an index,
changes configuration, or invokes a model. Failures retain their new evidence.
"""
import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import time

import frozen_task_eval as evaluation
import frozen_task_run as collection

MAX_BYTES = 1024 * 1024
TOOLS = {'get_codebase_overview', 'impact_analysis', 'compute_blast_radius'}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def plain(path):
    """Check every existing ancestor before resolving; junctions are not inputs."""
    path = Path(os.path.abspath(path))
    for parent in [path, *path.parents]:
        if parent.exists() or parent.is_symlink():
            info = parent.lstat()
            require(not stat.S_ISLNK(info.st_mode) and not
                    getattr(info, 'st_file_attributes', 0) & 0x400,
                    'Reparse points and symlinks are not capture paths: ' + str(parent))
    return path


def relative(root, name):
    require(isinstance(name, str) and bool(name), 'Empty relative path')
    part = PurePosixPath(name)
    require(not part.is_absolute() and not any(p in ('.', '..') for p in name.split('/'))
            and '\\' not in name and ':' not in name and str(part) == name,
            'Use normalized relative paths')
    path = plain(root / name)
    require(path.is_relative_to(root), 'Path escaped root')
    return path


def read(path, limit=MAX_BYTES):
    path = plain(path)
    require(path.is_file() and path.stat().st_size <= limit, 'Missing or oversized file: ' + str(path))
    with path.open('rb') as stream:
        raw = stream.read(limit + 1)
    require(len(raw) <= limit, 'File grew beyond bound')
    return raw


def file_hash(path, limit=512 * MAX_BYTES):
    import hashlib
    path = plain(path)
    require(path.is_file() and path.stat().st_size <= limit, 'Missing or oversized pinned file')
    value = hashlib.sha256()
    size = 0
    with path.open('rb') as stream:
        while block := stream.read(MAX_BYTES):
            size += len(block)
            require(size <= limit, 'Pinned file grew beyond bound')
            value.update(block)
    return value.hexdigest()


def process_identity(pid):
    require(os.name == 'nt', 'Current producer capture requires Windows')
    api = ctypes.WinDLL('kernel32', use_last_error=True)
    api.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    api.OpenProcess.restype = wintypes.HANDLE
    api.CloseHandle.argtypes = [wintypes.HANDLE]
    api.QueryFullProcessImageNameW.argtypes = [wintypes.HANDLE, wintypes.DWORD,
                                             wintypes.LPWSTR, ctypes.POINTER(wintypes.DWORD)]
    api.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    handle = api.OpenProcess(0x1000, False, pid)
    require(bool(handle), 'Pinned primary process is unavailable')
    try:
        buffer = ctypes.create_unicode_buffer(32768)
        length = wintypes.DWORD(len(buffer))
        require(api.QueryFullProcessImageNameW(handle, 0, buffer, ctypes.byref(length)),
                'Cannot identify primary executable')
        times = [wintypes.FILETIME() for _ in range(4)]
        require(api.GetProcessTimes(handle, *(ctypes.byref(t) for t in times)),
                'Cannot identify primary creation time')
        return dict(binary=str(plain(buffer.value)),
                    created_ticks=(times[0].dwHighDateTime << 32) | times[0].dwLowDateTime)
    finally:
        api.CloseHandle(handle)


class InstalledPipe:
    """One connection; observation ticks never reconnect or resend pending RPCs."""
    def __init__(self, socket, pid, emit, deadline=180):
        require(os.name == 'nt' and re.fullmatch(r'\\\\\.\\pipe\\engram-[A-Za-z0-9_-]+', socket),
                'Only an existing local Engram named pipe is allowed')
        import msvcrt
        self.stream = open(socket, 'r+b', buffering=0)
        self.handle = wintypes.HANDLE(msvcrt.get_osfhandle(self.stream.fileno()))
        self.api = ctypes.WinDLL('kernel32', use_last_error=True)
        self.api.GetNamedPipeServerProcessId.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.ULONG)]
        self.api.PeekNamedPipe.argtypes = [wintypes.HANDLE, wintypes.LPVOID, wintypes.DWORD,
                                         wintypes.LPVOID, ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID]
        self.emit, self.deadline, self.sequence = emit, deadline, 0
        try:
            observed = wintypes.ULONG()
            require(self.api.GetNamedPipeServerProcessId(self.handle, ctypes.byref(observed))
                    and observed.value == pid, 'Named pipe server PID differs from pin')
        except BaseException:
            self.close()
            raise

    def close(self):
        self.stream.close()

    def rpc(self, method, params):
        self.sequence += 1
        request = dict(jsonrpc='2.0', id=self.sequence, method=method, params=params)
        raw = collection.encoded(request).replace(b'\n', b' ') + b'\n'
        require(len(raw) <= 8192, 'Producer request exceeds 8 KiB control-frame bound')
        self.emit(f'rpc/{self.sequence:03d}-request.json', raw)
        require(self.stream.write(raw) == len(raw), 'Incomplete pipe write; no retry')
        pending = bytearray()
        received = 0
        started = time.monotonic()
        frame = 0
        try:
            while time.monotonic() - started < self.deadline:
                available = wintypes.DWORD()
                require(self.api.PeekNamedPipe(self.handle, None, 0, None,
                                               ctypes.byref(available), None), 'Producer pipe disconnected')
                if not available.value:
                    time.sleep(0.05)
                    continue  # Same pending request and connection, never a resend.
                chunk = self.stream.read(min(available.value, 65536))
                require(bool(chunk), 'Producer pipe closed')
                pending.extend(chunk)
                received += len(chunk)
                require(received <= MAX_BYTES, 'RPC response exceeds 1 MiB')
                while b'\n' in pending:
                    line, _, rest = pending.partition(b'\n')
                    pending = bytearray(rest)
                    frame += 1
                    require(frame <= 128, 'Too many producer frames for one pending RPC')
                    self.emit(f'rpc/{self.sequence:03d}-frame-{frame:03d}.json', bytes(line) + b'\n')
                    value = json.loads(line)
                    if value.get('id') == self.sequence:
                        require('error' not in value and 'result' in value, 'Producer JSON-RPC error')
                        return value['result']
                    require('id' not in value, 'Unexpected server request or response ID')
            raise ValueError('Producer RPC deadline expired; request was not resent')
        finally:
            if pending:
                self.emit(f'rpc/{self.sequence:03d}-partial.bin', bytes(pending[:MAX_BYTES]))

    def initialized(self):
        raw = b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n'
        self.emit('rpc/initialized.json', raw)
        require(self.stream.write(raw) == len(raw), 'Incomplete initialization notification')


def tool_text(result):
    require(isinstance(result, dict) and result.get('isError', False) is False,
            'Producer tool returned isError or invalid result')
    blocks = result.get('content')
    require(isinstance(blocks, list) and bool(blocks) and all(
        isinstance(b, dict) and b.get('type') == 'text' and isinstance(b.get('text'), str)
        for b in blocks), 'Producer response must contain complete text blocks')
    return '\n'.join(b['text'] for b in blocks)


def freshness(result, pins):
    text = tool_text(result)
    fields = dict(re.findall(r'^([a-z_]+): (.*)$', text, re.MULTILINE))
    require(fields.get('project_id') == pins['project_id'] and
            fields.get('active_generation') == str(pins['active_generation']) and
            plain(fields.get('directory', '')) == plain(pins['source_root']),
            'Producer project, source root, or generation differs from pins')
    require(fields.get('generation_complete', '').startswith('true (') and
            fields.get('source_index_format', '').startswith('current (') and
            'recovery_reason' not in fields and 'WARNING: recovery required' not in text,
            'Producer source generation is incomplete, unavailable, or needs recovery')


def prepare_current(spec_directory, destination, pins_path, *, transport=InstalledPipe,
                    identify=process_identity):
    spec_directory, destination = plain(spec_directory), plain(destination)
    require(spec_directory.is_dir() and not destination.exists() and
            not destination.is_relative_to(spec_directory), 'Capture needs a fresh external destination')
    spec_raw, pins_raw = read(spec_directory / 'spec.json'), read(pins_path)
    spec, pins = json.loads(spec_raw), json.loads(pins_raw)
    require(spec.get('version') == 1, 'Unsupported capture recipe version')
    cases = spec['cases']
    require(len(cases) == 4 and {c['category'] for c in cases} == evaluation.CATEGORIES and
            len({c['id'] for c in cases}) == 4, 'Exactly four distinct original categories are required')
    require(all(re.fullmatch(r'[A-Za-z0-9_-]{1,80}', c['id']) and
                isinstance(c['task'], str) and c['task'] for c in cases), 'Invalid case identity or task')
    requests = [r for c in cases for r in c['tool_requests']]
    require(1 <= len(requests) <= 32 and all(c['tool_requests'] for c in cases), 'Capture request count is invalid')
    for request in requests:
        require(set(request) == {'name', 'arguments'} and request['name'] in TOOLS and
                isinstance(request['arguments'], dict) and
                request['arguments'].get('project_id') == pins['project_id'] and
                len(collection.encoded(request)) <= 4096,
                'Only the three read-only core tools for the pinned project are allowed')
    inputs = {spec['oracle'], *spec.get('frozen_metadata', [])}
    for case in cases:
        require(isinstance(case['available_reads'], dict) and len(case['available_reads']) <= 64 and
                isinstance(case['base_inputs'], list) and bool(case['base_inputs']), 'Invalid bounded source inputs')
        inputs.update(case['base_inputs'])
        inputs.update(case['available_reads'].values())
    require(len(inputs) <= 256, 'Too many frozen source inputs')
    input_bytes = {name: read(relative(spec_directory, name)) for name in inputs}
    require(sum(map(len, input_bytes.values())) <= 16 * MAX_BYTES, 'Source packets exceed 16 MiB')
    parsed_inputs = {name: json.loads(raw) for name, raw in input_bytes.items() if name not in spec.get('frozen_metadata', [])}
    oracle = parsed_inputs[spec['oracle']]
    require(isinstance(oracle.get('judge_only_canary'), str) and bool(oracle['judge_only_canary']) and
            set(oracle['cases']) == {c['id'] for c in cases} and
            all(oracle['cases'][c['id']].get('criteria') for c in cases), 'Freeze complete criteria before capture')
    canary = oracle['judge_only_canary'].encode()
    require(all(canary not in collection.encoded(parsed_inputs[name]) for c in cases
                for name in [*c['base_inputs'], *c['available_reads'].values()]), 'Oracle canary leaked into source input')
    sources = spec['source_files']
    require(isinstance(sources, dict) and 1 <= len(sources) <= 512 and all(
        re.fullmatch('[0-9a-f]{64}', h) for h in sources.values()), 'Pin independently prepared source hashes')
    root = plain(pins['source_root'])
    require(root.is_dir() and not destination.is_relative_to(root), 'Capture must be outside current source root')
    destination.mkdir()
    evidence = {}
    def emit(name, raw):
        require(len(raw) <= MAX_BYTES, 'Capture artifact exceeds 1 MiB')
        path = relative(destination, name)
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open('xb') as stream:
            stream.write(raw)
        evidence[name] = collection.digest(raw)
    emit('recipe/spec.json', spec_raw)
    emit('provenance/pins.json', pins_raw)
    for name, raw in input_bytes.items():
        emit('recipe/' + name, raw)
    state = dict(status='capturing', kind='current_producer', started_at=collection.now(),
                 retention='Retain failed and completed private evidence for explicit review and cleanup.',
                 limits='Current producer capture is not a fresh holdout or implementation/productivity result. '
                        'Source hashes cover only the frozen selected source files; disk-wide freshness is unknown.')
    emit('provenance/capture-start.json', collection.encoded(state))
    pipe = None
    try:
        def snapshot():
            require(file_hash(pins['binary']) == pins['binary_sha256'] and
                    file_hash(pins['build_manifest']) == pins['build_manifest_sha256'], 'Installed build or manifest hash drift')
            metadata_raw = read(Path(pins['primary_metadata']))
            metadata = dict(line.split('=', 1) for line in metadata_raw.decode().splitlines() if '=' in line)
            require(metadata.get('pid') == str(pins['primary_pid']) and
                    metadata.get('started_at_ms') == str(pins['primary_started_at_ms']) and
                    metadata.get('protocol_version') == '1', 'Primary metadata differs from pins')
            identity = identify(pins['primary_pid'])
            require(plain(identity['binary']) == plain(pins['binary']), 'Primary executable differs from pin')
            hashes = {name: file_hash(relative(root, name), 16 * MAX_BYTES) for name in sources}
            require(hashes == sources, 'Independent source evidence changed before/during capture')
            return dict(metadata_sha256=collection.digest(metadata_raw), metadata=metadata,
                        process=identity, source_files=hashes)
        before = snapshot()
        emit('provenance/before.json', collection.encoded(before))
        emit('provenance/build-manifest.json', read(Path(pins['build_manifest'])))
        pipe = transport(before['metadata']['socket'], pins['primary_pid'], emit)
        initialized = pipe.rpc('initialize', dict(protocolVersion='2024-11-05', capabilities={},
                               clientInfo=dict(name='engram-frozen-current-capture', version='1')))
        emit('provenance/initialize.json', collection.encoded(initialized))
        pipe.initialized()
        listed = pipe.rpc('tools/list', {})
        emit('provenance/tools-list.json', collection.encoded(listed))
        require(not listed.get('nextCursor') and TOOLS | {'get_index_freshness'} <=
                {t['name'] for t in listed['tools']}, 'Incomplete installed tools/list')
        def call(request, name):
            result = pipe.rpc('tools/call', request)
            emit(name, collection.encoded(result))
            tool_text(result)
            return result
        fresh_request = dict(name='get_index_freshness', arguments=dict(project_id=pins['project_id'], check_disk=False))
        freshness(call(fresh_request, 'provenance/freshness-before.json'), pins)
        manifest_cases = []
        for case in cases:
            base = [parsed_inputs[n] for n in case['base_inputs']]
            captured = []
            for number, request in enumerate(case['tool_requests']):
                require(snapshot() == before, 'Process, build, source or primary metadata drift')
                name = f"producer/{case['id']}-{number}.json"
                result = call(request, name)
                captured.append(dict(request=request, response=result,
                                     response_bytes=len(collection.encoded(result)), response_sha256=evidence[name]))
            baseline, assisted = f"inputs/{case['id']}-baseline.json", f"inputs/{case['id']}-assisted.json"
            emit(baseline, collection.encoded(dict(source_inputs=base)))
            emit(assisted, collection.encoded(dict(source_inputs=base, tool_responses=captured)))
            manifest_cases.append(dict(id=case['id'], category=case['category'], task=case['task'],
                available_reads={k: 'recipe/' + v for k, v in case['available_reads'].items()},
                arms=dict(baseline={'input': baseline}, assisted={'input': assisted})))
        freshness(call(fresh_request, 'provenance/freshness-after.json'), pins)
        after = snapshot()
        emit('provenance/after.json', collection.encoded(after))
        require(after == before, 'Process, build, source or primary metadata drift')
        # A failed observer shutdown must never leave a sealed successful corpus.
        closing, pipe = pipe, None
        closing.close()
        state.update(status='complete', finished_at=collection.now())
        emit('provenance/capture-complete.json', collection.encoded(state))
        manifest = dict(version=1, producer_kind='current_producer', cases=manifest_cases,
                        judge_oracle='recipe/' + spec['oracle'], frozen_files=dict(evidence),
                        producer_provenance='provenance/capture-complete.json')
        emit('manifest.json', collection.encoded(manifest))
        emit('freeze_record.json', collection.encoded(dict(manifest_sha256=evidence['manifest.json'],
             producer_kind='current_producer', frozen_at=collection.now())))
        evaluation.load(destination)
        return dict(status='current_producer_frozen', directory=str(destination),
                    cases=4, arms=8, producer_requests=len(requests), limits=state['limits'])
    except BaseException as error:
        emit('capture-failure.json', collection.encoded(dict(status='failed', error=str(error),
             finished_at=collection.now(), retained_files=evidence)))
        raise
    finally:
        if pipe is not None:
            pipe.close()
