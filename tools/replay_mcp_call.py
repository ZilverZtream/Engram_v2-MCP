"""Call an isolated replay broker with visible errors and basic schema checks.

Usage: replay_mcp_call.py RUNTIME list [term] | schema TOOL | TOOL @arguments.json
This is an evaluation adapter, not an Engram server or full JSON Schema validator.
"""
import json
from pathlib import Path
import sys
import urllib.error
import urllib.request


def emit(value):
    # ASCII JSON works even under legacy Windows stdout encodings. Parsing the
    # JSON restores Unicode; do not require callers to set PYTHONIOENCODING.
    print(json.dumps(value, ensure_ascii=True))


def load_json(path):
    raw = Path(path).read_bytes()
    encoding = 'utf-16' if raw.startswith((b'\xff\xfe', b'\xfe\xff')) else 'utf-8-sig'
    return json.loads(raw.decode(encoding))


def type_matches(value, kind):
    if kind == 'null': return value is None
    if kind == 'boolean': return isinstance(value, bool)
    if kind == 'integer': return isinstance(value, int) and not isinstance(value, bool)
    if kind == 'number': return isinstance(value, (int, float)) and not isinstance(value, bool)
    if kind == 'string': return isinstance(value, str)
    if kind == 'object': return isinstance(value, dict)
    if kind == 'array': return isinstance(value, list)
    return True


def check_arguments(arguments, schema):
    if not isinstance(arguments, dict):
        raise ValueError('Tool arguments must be a JSON object')
    properties = schema.get('properties', {})
    for key in schema.get('required', []):
        if key not in arguments:
            raise ValueError(f'Missing required argument: {key}')
    for key, value in arguments.items():
        if key not in properties:
            if schema.get('additionalProperties') is False:
                raise ValueError(f'Unknown argument: {key}')
            continue
        spec = properties[key]
        kinds = spec.get('type')
        if kinds is None and 'anyOf' in spec:
            branches = spec['anyOf']
            # Do not reject valid inputs where a branch uses references or
            # constraints this intentionally small preflight does not resolve.
            if all('type' in branch for branch in branches):
                kinds = [branch['type'] for branch in branches]
        if kinds is None:
            continue
        kinds = [kinds] if isinstance(kinds, str) else kinds
        if not any(type_matches(value, kind) for kind in kinds):
            hint = ''
            if isinstance(value, dict) and 'value' in value and 'PSPath' in value:
                hint = (' PowerShell serialized an extended string object. '
                        'Read text with [System.IO.File]::ReadAllText(path) before ConvertTo-Json; '
                        'do not silently coerce this object.')
            raise ValueError(f'Argument {key!r} must be {" or ".join(kinds)}; got {type(value).__name__}.{hint}')


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) < 2:
        emit({'error':__doc__.strip()})
        return 2
    runtime, name = Path(argv[0]), argv[1]
    try:
        schemas = load_json(runtime / 'schemas.json')
        if name == 'list':
            term = argv[2].lower() if len(argv) > 2 else ''
            emit([{'name':k,'description':v.get('description','')} for k,v in schemas.items()
                  if term in (k+' '+v.get('description','')).lower()])
            return 0
        if name == 'schema':
            if len(argv) < 3: raise ValueError('schema requires a tool name')
            emit(schemas[argv[2]])
            return 0
        raw = argv[2] if len(argv) > 2 else '{}'
        arguments = load_json(raw[1:]) if raw.startswith('@') else json.loads(raw)
        check_arguments(arguments, schemas[name].get('inputSchema', {}))
        url = load_json(runtime / 'connection.json')['url']
        request = urllib.request.Request(url + '/call',
            data=json.dumps({'tool':name,'arguments':arguments}).encode('utf-8'),
            headers={'Content-Type':'application/json'})
        try:
            with urllib.request.urlopen(request, timeout=7200) as response:
                result = json.load(response)
        except urllib.error.HTTPError as error:
            body = error.read(128 * 1024).decode('utf-8', errors='replace')
            try: detail = json.loads(body)
            except json.JSONDecodeError: detail = body
            emit({'http_status':error.code,'error':detail})
            return 1
        emit(result)
        return 1 if result.get('isError') else 0
    except (ValueError, KeyError, OSError) as error:
        emit({'error':str(error)})
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
