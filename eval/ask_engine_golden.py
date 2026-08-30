#!/usr/bin/env python3
"""Seed golden eval for the rebuilt ask_codebase (Milestone 1).

Drives the DEPLOYED daemon (so build + deploy the new binary first, per the
deploy runbook) and scores each corpus question on the M1 gate:

  * abstain 100% on missing_knowledge rows (must return status=unsupported),
  * status-match >= 80% across the corpus,
  * citation coverage on answerable rows (fraction of `must_cite_any` substrings
    that appear in some evidence path/content),
  * latency,
  * CORRECTNESS (round-2 audit P0-4): every row is judged on its evidence —
    required_modality (>= 1 evidence item of the requested kind), required_all
    (symbols/files that must all be cited), forbidden (distractors that must not
    be cited) and min_precision; an answered row with zero evidence of the
    requested modality FAILS. The gate needs 100 % correct rows.
  --out <path> writes the per-row record (JSON) for the acceptance dossier.

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
    # Always sent explicitly so the record says which arm set produced it.
    body = json.loads(payload)
    body["include_insights"] = bool(include_insights)
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


def path_matches(path: str, token: str) -> bool:
    """'.ext' = suffix, 'pr:'-style = prefix, anything else = substring."""
    p, t = path.lower(), token.lower()
    if t.startswith("."):
        return p.endswith(t)
    if t.endswith(":"):
        return p.startswith(t)
    return t in p


def judge(row: dict, status: str, report: dict) -> tuple[bool, str]:
    """Round-2 audit P0-4: is this row CORRECT, not merely status-labelled?"""
    expect = row["expect_status"]
    if status not in expect:
        return False, f"status {status} not in {expect}"
    if row.get("must_abstain"):
        return (True, "") if status == "unsupported" else (False, "did not abstain")
    if status not in ("answered", "partial"):
        return True, ""  # an allowed abstention/ambiguity makes no evidence claim
    ev = report.get("evidence", [])
    paths = [str(e.get("path", "")) for e in ev]
    if not ev:
        return False, "answered with zero evidence"
    mod = row.get("required_modality", [])
    if mod and not any(path_matches(p, t) for p in paths for t in mod):
        return False, f"no evidence of the requested modality {mod}"
    blob = evidence_blob(report)
    missing = [t for t in row.get("required_all", []) if t.lower() not in blob]
    if missing:
        return False, f"required symbols/files not cited: {missing}"
    bad = [p for p in paths if any(path_matches(p, t) for t in row.get("forbidden", []))]
    if bad:
        return False, f"forbidden distractor cited: {bad[:3]}"
    mp = row.get("min_precision")
    if mp is not None:
        toks = set(row.get("required_all", [])) | set(row.get("must_cite_any", [])) | set(mod)
        rel = sum(
            1
            for e in ev
            if any(
                t.lower() in (str(e.get("path", "")) + " " + str(e.get("content", ""))).lower()
                for t in toks
            )
        )
        prec = rel / len(ev)
        if prec < mp:
            return False, f"evidence precision {prec:.2f} < {mp}"
    return True, ""


def evidence_blob(report: dict) -> str:
    parts = []
    for e in report.get("evidence", []):
        parts.append(str(e.get("path", "")))
        parts.append(str(e.get("content", "")))
    return " ".join(parts).lower()


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: python eval/ask_engine_golden.py <project_id> [corpus.jsonl] [--insights] [--out record.json]")
        return 2
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    out_path = None
    if "--out" in sys.argv:
        out_path = sys.argv[sys.argv.index("--out") + 1]
        args = [a for a in args if a != out_path]
    # Round-2 audit P1-4: the arm is OFF by default; --insights turns it on.
    include_insights = "--insights" in sys.argv
    project_id = args[0]
    corpus_path = Path(args[1]) if len(args) > 1 else CORPUS
    rows = [json.loads(ln) for ln in corpus_path.read_text(encoding="utf-8").splitlines() if ln.strip()]

    status_matches = 0
    abstain_total = 0
    abstain_ok = 0
    correct = 0
    failures = []
    records = []
    cite_scores = []
    total_latency = 0.0
    insight_rows = 0
    insight_items = 0

    print(f"{'id':<18}{'category':<16}{'status':<13}{'exp':<20}{'cite':<6}{'ms':>6}  verdict")
    print("-" * 96)
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

        ok, reason = judge(row, status, report)
        if ok:
            correct += 1
        else:
            failures.append((row["id"], reason))
        records.append(
            {
                "id": row["id"],
                "category": row["category"],
                "question": row["question"],
                "status": status,
                "expect_status": expect,
                "correct": ok,
                "reason": reason,
                "cite_coverage": cite_s,
                "ms": int(dt * 1000),
                "evidence_paths": [str(e.get("path", "")) for e in report.get("evidence", [])][:12],
            }
        )
        exp_s = "/".join(expect)
        print(
            f"{row['id']:<18}{row['category']:<16}{status:<13}{exp_s:<20}{cite_s:<6}{int(dt*1000):>6}  "
            f"{'ok' if ok else 'FAIL: ' + reason}"
        )

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

    print(f"correct:      {correct}/{n} = {correct / n if n else 0:.0%}   (gate = 100%: status + required modality + required symbols + no forbidden distractor + precision)")
    for fid, reason in failures:
        print(f"  FAIL {fid}: {reason}")

    gate_ok = abstain_rate >= 1.0 and status_rate >= 0.80 and correct == n
    print(f"\nGATE: {'PASS' if gate_ok else 'FAIL'}")
    if out_path:
        summary = {
            "corpus": str(corpus_path),
            "project_id": project_id,
            "rows": n,
            "status_match": status_matches,
            "abstain_ok": abstain_ok,
            "abstain_total": abstain_total,
            "correct": correct,
            "mean_cite": round(mean_cite, 3),
            "gate": "PASS" if gate_ok else "FAIL",
            "insights": include_insights,
            "finished_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        }
        Path(out_path).write_text(
            json.dumps({"summary": summary, "rows": records}, indent=1, ensure_ascii=False),
            encoding="utf-8",
        )
        print(f"record written: {out_path}")
    return 0 if gate_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
