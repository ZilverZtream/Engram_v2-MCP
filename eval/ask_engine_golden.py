#!/usr/bin/env python3
"""Seed golden eval for the rebuilt ask_codebase (Milestone 1).

Drives the DEPLOYED daemon (so build + deploy the new binary first, per the
deploy runbook) and scores each corpus question on the M1 gate:

  * abstain 100% on missing_knowledge rows (must return status=unsupported),
  * status-match >= 80% across the corpus,
  * citation coverage on answerable rows (fraction of `must_cite_any` substrings
    that appear in some evidence path/content),
  * latency.

Usage:
  python eval/ask_engine_golden.py <project_id> [corpus.jsonl]

The runner asks ask_codebase for output_format="json" and parses the AskReport
(status, evidence[].path / .content). It scores; it does not mutate anything.
"""

import json
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
DRIVE = REPO / "tools" / "engram_drive.py"
CORPUS = REPO / "eval" / "ask_engine_golden.jsonl"


def ask(project_id: str, question: str) -> tuple[dict, float]:
    """Call ask_codebase (json output) via engram_drive; return (report, secs)."""
    payload = json.dumps(
        {
            "project_id": project_id,
            "question": question,
            "output_format": "json",
            "depth": "standard",
        }
    )
    t0 = time.time()
    proc = subprocess.run(
        [sys.executable, str(DRIVE), "tool", "ask_codebase", payload],
        capture_output=True,
        text=True,
        encoding="utf-8",
        cwd=str(REPO),
    )
    dt = time.time() - t0
    out = proc.stdout.strip()
    # engram_drive prints the tool's text content; with output_format=json that
    # text IS the report JSON. Be tolerant of a leading log line.
    start = out.find("{")
    if start < 0:
        return {"status": "PARSE_ERROR", "_raw": out[:400], "_stderr": proc.stderr[:400]}, dt
    try:
        return json.loads(out[start:]), dt
    except json.JSONDecodeError:
        return {"status": "PARSE_ERROR", "_raw": out[:400]}, dt


def evidence_blob(report: dict) -> str:
    parts = []
    for e in report.get("evidence", []):
        parts.append(str(e.get("path", "")))
        parts.append(str(e.get("content", "")))
    return " ".join(parts).lower()


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: python eval/ask_engine_golden.py <project_id> [corpus.jsonl]")
        return 2
    project_id = sys.argv[1]
    corpus_path = Path(sys.argv[2]) if len(sys.argv) > 2 else CORPUS
    rows = [json.loads(ln) for ln in corpus_path.read_text(encoding="utf-8").splitlines() if ln.strip()]

    status_matches = 0
    abstain_total = 0
    abstain_ok = 0
    cite_scores = []
    total_latency = 0.0

    print(f"{'id':<14}{'category':<16}{'status':<13}{'exp':<13}{'cite':<6}{'ms':>6}")
    print("-" * 74)
    for row in rows:
        report, dt = ask(project_id, row["question"])
        total_latency += dt
        status = report.get("status", "ERROR")
        expect = row["expect_status"]
        if status == expect:
            status_matches += 1

        if row.get("must_abstain"):
            abstain_total += 1
            if status == "unsupported":
                abstain_ok += 1

        cites = row.get("must_cite_any", [])
        if cites:
            blob = evidence_blob(report)
            hit = sum(1 for c in cites if c.lower() in blob)
            cov = hit / len(cites)
            cite_scores.append(cov)
            cite_s = f"{cov:.2f}"
        else:
            cite_s = "-"

        print(f"{row['id']:<14}{row['category']:<16}{status:<13}{expect:<13}{cite_s:<6}{int(dt*1000):>6}")

    n = len(rows)
    print("-" * 74)
    status_rate = status_matches / n if n else 0.0
    abstain_rate = (abstain_ok / abstain_total) if abstain_total else 1.0
    mean_cite = (sum(cite_scores) / len(cite_scores)) if cite_scores else 0.0
    print(f"status-match: {status_matches}/{n} = {status_rate:.0%}   (gate >= 80%)")
    print(f"abstain:      {abstain_ok}/{abstain_total} = {abstain_rate:.0%}   (gate = 100%)")
    print(f"mean citation coverage (answerable rows): {mean_cite:.2f}")
    print(f"mean latency: {int(total_latency/n*1000)} ms")

    gate_ok = abstain_rate >= 1.0 and status_rate >= 0.80
    print(f"\nGATE: {'PASS' if gate_ok else 'FAIL'}")
    return 0 if gate_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
