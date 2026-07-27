"""Seal a subagent's final planning output as a prediction file.

Reads the task transcript JSONL, extracts the LAST assistant text block,
and writes it verbatim to the prediction path. Content never passes
through the orchestrator's context - the seal is byte-faithful.
"""
import json
import sys

task_file, out_path = sys.argv[1], sys.argv[2]
last_text = None
with open(task_file, encoding='utf-8', errors='replace') as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        try:
            m = json.loads(line)
        except json.JSONDecodeError:
            continue
        msg = m.get('message', m)
        if msg.get('role') != 'assistant':
            continue
        content = msg.get('content')
        if isinstance(content, list):
            texts = [c.get('text', '') for c in content if c.get('type') == 'text']
            if texts and texts[-1].strip():
                last_text = '\n'.join(t for t in texts if t.strip())
        elif isinstance(content, str) and content.strip():
            last_text = content

if not last_text:
    print('NO TEXT FOUND')
    sys.exit(1)
with open(out_path, 'w', encoding='utf-8') as f:
    f.write(last_text + '\n')
print(f'sealed {out_path} ({len(last_text)} chars)')
