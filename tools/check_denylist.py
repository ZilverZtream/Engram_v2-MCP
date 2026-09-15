"""Reject customer-identifying strings in shipped source.

Engram is codebase-agnostic, so customer names, schema vocabulary and replay
identifiers must never reach product code, comments or tests. The denylist
lives OUTSIDE the repository (so this check never ships the names it guards
against): one case-insensitive regex per line, blank lines and `#` comments
ignored.

Denylist location, first match wins: --denylist, $ENGRAM_DENYLIST,
~/.config/engram/denylist.txt. A missing denylist warns and passes, so the
check never blocks a machine that has not been configured.

    python tools/check_denylist.py --staged crates rule-packs tools   # pre-commit: added lines only
    python tools/check_denylist.py --tree crates                      # audit: every tracked line

Exit codes: 0 clean, 1 matches found, 2 invalid denylist.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys

DEFAULT_DENYLIST = Path.home() / '.config' / 'engram' / 'denylist.txt'
HUNK = re.compile(r'^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@')


def load_patterns(path):
    patterns = []
    for number, raw in enumerate(path.read_text(encoding='utf-8').splitlines(), start=1):
        line = raw.strip()
        if not line or line.startswith('#'):
            continue
        try:
            patterns.append(re.compile(line, re.IGNORECASE))
        except re.error as error:
            print(f'{path}:{number}: invalid denylist pattern {line!r}: {error}', file=sys.stderr)
            sys.exit(2)
    return patterns


def git(repo, *args):
    return subprocess.run(['git', '-C', str(repo), '-c', 'core.quotepath=off', *args],
                          capture_output=True, check=True).stdout


def staged_lines(repo, scope):
    diff = git(repo, 'diff', '--cached', '-U0', '--no-color', '--diff-filter=ACMR', '--', *scope)
    path, number, previous = None, 0, ''
    for line in diff.decode('utf-8', errors='replace').splitlines():
        if line.startswith('+++ ') and previous.startswith('--- '):
            path = line[6:] if line.startswith('+++ b/') else None
        elif match := HUNK.match(line):
            number = int(match.group(1))
        elif path and line.startswith('+'):
            yield path, number, line[1:]
            number += 1
        previous = line


def tracked_lines(repo, scope):
    listing = git(repo, 'ls-files', '-z', '--', *scope)
    for rel in filter(None, listing.decode('utf-8', errors='replace').split('\0')):
        data = (Path(repo) / rel).read_bytes()
        if b'\0' in data[:8000]:
            continue
        for number, line in enumerate(data.decode('utf-8', errors='replace').splitlines(), start=1):
            yield rel, number, line


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--staged', action='store_true', help='check lines added in the index')
    mode.add_argument('--tree', action='store_true', help='check every tracked line')
    parser.add_argument('--repo', default='.')
    parser.add_argument('--denylist')
    parser.add_argument('scope', nargs='*', help='path prefixes to check (default: whole repository)')
    args = parser.parse_args()
    sys.stdout.reconfigure(errors='backslashreplace')
    sys.stderr.reconfigure(errors='backslashreplace')

    denylist = Path(args.denylist or os.environ.get('ENGRAM_DENYLIST') or DEFAULT_DENYLIST)
    if not denylist.is_file():
        print(f'engram denylist not found at {denylist}; customer-string check skipped', file=sys.stderr)
        return 0
    patterns = load_patterns(denylist)

    lines = staged_lines(args.repo, args.scope) if args.staged else tracked_lines(args.repo, args.scope)
    hits = 0
    for path, number, text in lines:
        for pattern in patterns:
            if pattern.search(text):
                print(f'{path}:{number}: matches denylist pattern {pattern.pattern!r}')
                hits += 1
                break
    if hits:
        print(f'{hits} line(s) contain customer-identifying strings; anonymize them '
              '(invent same-shape names) before committing.', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
