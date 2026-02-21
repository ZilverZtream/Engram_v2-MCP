#!/usr/bin/env python3
"""Validate generated capabilities matrices in docs against code capability flags."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
FLAGS_FILE = REPO_ROOT / "crates/engram_server/src/capabilities.rs"
DOC_FILES = [
    REPO_ROOT / "docs/TOOL_PARITY.md",
    REPO_ROOT / "docs/DEVELOPER_SPEC.md",
    REPO_ROOT / "docs/ROADMAP.md",
]

START_MARKER = "<!-- CAPABILITIES_MATRIX:START -->"
END_MARKER = "<!-- CAPABILITIES_MATRIX:END -->"


def parse_flags(path: Path) -> list[tuple[str, str]]:
    text = path.read_text(encoding="utf-8")
    pattern = re.compile(
        r'CapabilityFlag\s*\{\s*key:\s*"([^"]+)",\s*status:\s*CapabilityStatus::(Implemented|Partial|Experimental|Planned),?\s*\}',
        re.MULTILINE,
    )
    rows: list[tuple[str, str]] = []
    seen: set[str] = set()
    for key, status in pattern.findall(text):
        if key in seen:
            raise ValueError(f"duplicate code capability key: {key}")
        seen.add(key)
        rows.append((key, status.lower()))
    if not rows:
        raise ValueError(f"no capability flags found in {path}")
    return rows


def render_matrix(rows: list[tuple[str, str]]) -> str:
    lines = [
        START_MARKER,
        "| Tool / Feature | Status |",
        "| :--- | :--- |",
    ]
    for key, status in rows:
        lines.append(f"| `{key}` | {status} |")
    lines.append(END_MARKER)
    return "\n".join(lines)


def replace_matrix(content: str, matrix_block: str, path: Path) -> str:
    if START_MARKER not in content or END_MARKER not in content:
        raise ValueError(
            f"{path} must contain {START_MARKER} and {END_MARKER} markers"
        )
    pattern = re.compile(
        rf"{re.escape(START_MARKER)}.*?{re.escape(END_MARKER)}",
        flags=re.S,
    )
    return pattern.sub(matrix_block, content, count=1)


def check_or_write(rows: list[tuple[str, str]], write: bool) -> int:
    matrix_block = render_matrix(rows)
    drifted: list[Path] = []

    for doc in DOC_FILES:
        original = doc.read_text(encoding="utf-8")
        updated = replace_matrix(original, matrix_block, doc)
        if original != updated:
            if write:
                doc.write_text(updated, encoding="utf-8")
            else:
                drifted.append(doc)

    if drifted:
        print("Capability matrix drift detected in docs:")
        for doc in drifted:
            print(f"- {doc.relative_to(REPO_ROOT)}")
        print("Run: python3 scripts/check_capabilities_matrix.py --write")
        return 1

    mode = "updated" if write else "passed"
    print(f"Capability matrix check {mode} ({len(rows)} capabilities).")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--write", action="store_true", help="rewrite doc matrices")
    args = parser.parse_args()

    rows = parse_flags(FLAGS_FILE)
    return check_or_write(rows, write=args.write)


if __name__ == "__main__":
    sys.exit(main())
