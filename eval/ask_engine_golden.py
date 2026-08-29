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


def ask(project_id: str, question: str, include_insights: bool = True) -> tuple[dict, float]:
    """Call ask_codebase (json output) via engram_drive; return (report, secs)."""
    payload = json.dumps(
        {
            "project_id": project_id,
            "question": question,
            "output_format": "json",
            "depth": "standard",
            # Dream ablation switch (external audit 2026-08-29): False removes
            # the dreamer-insight retrieval arm and nothing else.
        }
    )
    if not include_insights:
        # Only sent when the ablation asks for it: servers before the switch
        # reject unknown fields, and the default is on anyway.
        body = json.loads(payload)
        body["include_insights"] = False
        payload = json.dumps(body)
    t0 = time.time()
    proc = subprocess.run(
        # 4th arg is engram_drive's output char cap; default 8000 would truncate
        # a JSON report into invalid JSON, so pass a generous cap.
        [sys.executable, str(DRIVE), "tool", "ask_codebase", payload, "400000"],
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
        print("usage: python eval/ask_engine_golden.py <project_id> [corpus.jsonl] [--no-insights]")
        return 2
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    include_insights = "--no-insights" not in sys.argv
    project_id = args[0]
    corpus_path = Path(args[1]) if len(args) > 1 else CORPUS
    rows = [json.loads(ln) for ln in corpus_path.read_text(encoding="utf-8").splitlines() if ln.strip()]

    status_matches = 0
    abstain_total = 0
    abstain_ok = 0
    cite_scores = []
    total_latency = 0.0
    insight_rows = 0
    insight_items = 0

    print(f"{'id':<14}{'category':<16}{'status':<13}{'exp':<13}{'cite':<6}{'ms':>6}")
    print("-" * 74)
    for row in rows:
        report, dt = ask(project_id, row["question"], include_insights)
        total_latency += dt
        n_ins = sum(1 for e in report.get("evidence", []) if e.get("authority") == "DreamerInsight")
        insight_items += n_ins
        insight_rows += 1 if n_ins else 0
        status = report.get("status", "ERROR")
        expect = row["expect_status"]  # list of acceptable statuses
        if status in expect:
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

        exp_s = "/".join(expect)
        print(f"{row['id']:<14}{row['category']:<16}{status:<13}{exp_s:<20}{cite_s:<6}{int(dt*1000):>6}")

    n = len(rows)
    print("-" * 74)
    status_rate = status_matches / n if n else 0.0
    abstain_rate = (abstain_ok / abstain_total) if abstain_total else 1.0
    mean_cite = (sum(cite_scores) / len(cite_scores)) if cite_scores else 0.0
    print(f"status-match: {status_matches}/{n} = {status_rate:.0%}   (gate >= 80%)")
    print(f"abstain:      {abstain_ok}/{abstain_total} = {abstain_rate:.0%}   (gate = 100%)")
    print(f"mean citation coverage (answerable rows): {mean_cite:.2f}")
    print(f"mean latency: {int(total_latency/n*1000)} ms")
    print(f"dreamer insights: {'ON' if include_insights else 'OFF'}; insight evidence in {insight_rows}/{n} rows ({insight_items} items)")

    gate_ok = abstain_rate >= 1.0 and status_rate >= 0.80
    print(f"\nGATE: {'PASS' if gate_ok else 'FAIL'}")
    return 0 if gate_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
