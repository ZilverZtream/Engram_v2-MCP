#!/usr/bin/env python3
"""Validate docs capability matrix against code capability flags."""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
FLAGS_FILE = REPO_ROOT / "crates/engram_server/src/capabilities.rs"
MATRIX_FILE = REPO_ROOT / "docs/TOOL_PARITY.md"

VALID_STATUSES = {"implemented", "partial", "experimental", "planned"}


def parse_flags(path: Path) -> dict[str, str]:
    text = path.read_text(encoding="utf-8")
    pattern = re.compile(
        r'CapabilityFlag\s*\{\s*key:\s*"([^"]+)",\s*status:\s*CapabilityStatus::(Implemented|Partial|Experimental|Planned),?\s*\}',
        re.MULTILINE,
    )
    flags: dict[str, str] = {}
    for key, status in pattern.findall(text):
        lowered = status.lower()
        if key in flags:
            raise ValueError(f"duplicate code capability key: {key}")
        flags[key] = lowered
    return flags


def parse_matrix(path: Path) -> dict[str, str]:
    text = path.read_text(encoding="utf-8")
    matrix: dict[str, str] = {}
    for line in text.splitlines():
        if not line.startswith("|"):
            continue
        parts = [p.strip() for p in line.strip().split("|")[1:-1]]
        if len(parts) < 3:
            continue
        if parts[0] in {":---", "Tool / Feature"}:
            continue
        key = parts[0].strip("`")
        status = parts[1].lower()
        if status not in VALID_STATUSES:
            raise ValueError(f"invalid status '{status}' for '{key}' in {path}")
        if key in matrix:
            raise ValueError(f"duplicate matrix capability key: {key}")
        matrix[key] = status
    return matrix


def main() -> int:
    flags = parse_flags(FLAGS_FILE)
    matrix = parse_matrix(MATRIX_FILE)

    missing_in_docs = sorted(set(flags) - set(matrix))
    missing_in_code = sorted(set(matrix) - set(flags))

    mismatches: list[tuple[str, str, str]] = []
    for key in sorted(set(flags) & set(matrix)):
        if flags[key] != matrix[key]:
            mismatches.append((key, flags[key], matrix[key]))

    if missing_in_docs or missing_in_code or mismatches:
        print("Capability matrix drift detected:")
        if missing_in_docs:
            print("- Missing in docs/TOOL_PARITY.md:")
            for key in missing_in_docs:
                print(f"  - {key}")
        if missing_in_code:
            print("- Missing in crates/engram_server/src/capabilities.rs:")
            for key in missing_in_code:
                print(f"  - {key}")
        if mismatches:
            print("- Status mismatches:")
            for key, code_status, doc_status in mismatches:
                print(f"  - {key}: code={code_status}, docs={doc_status}")
        return 1

    print(f"Capability matrix check passed ({len(flags)} capabilities).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
