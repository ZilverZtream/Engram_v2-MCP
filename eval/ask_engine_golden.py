#!/usr/bin/env python3
"""Golden eval v3 for ask_codebase — ITEM-LEVEL correctness (doc 11, P0-2).

The pre-audit proved the v2 judge awarded a live false positive: required_all
searched every evidence path and body as ONE blob, so a checklist document
containing "ioMarkerInfowindow" plus an unrelated .d.ts satisfied a question
about ts/map renderers. v3 judges per evidence ITEM:

  * required_items — each {path_suffix|path_any, content_all, kind} predicate
    must be satisfied by a SINGLE evidence item (path match AND every content
    token inside THAT item's path+content).
  * required_all / must_cite_any — a token counts only when it appears within
    ONE item (its path + its own content), never across the concatenation.
  * item-level precision excluding extension tokens (".ts" proves nothing) —
    default min_precision 0.34 on answered rows, per-row override.
  * file-set recall — answer_files + min_file_recall for "which files" rows.
  * forbidden_classes — evidence classes that must not be cited (path
    substrings, e.g. "typings/", "memory_bank:", ".coderabbit").
  * abstention labels are honest: a non-abstain row answered with
    unsupported/ambiguous FAILS unless the row sets allow_abstain: true.

Gate: abstain 100%, status-match >= 80%, correct == rows (100%).
--out writes the per-row record with the corpus sha256 (dossier).

Usage:
  python eval/ask_engine_golden.py <project_id> [corpus.jsonl] [--insights] [--out record.json]
"""

import hashlib
import json
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
DRIVE = REPO / "tools" / "engram_drive.py"
CORPUS = REPO / "eval" / "ask_engine_golden.jsonl"

JUDGE_VERSION = "v3-item-level"
DEFAULT_MIN_PRECISION = 0.34


def ask(project_id: str, question: str, include_insights: bool = True) -> tuple[dict, float]:
    """Call ask_codebase (json output) via engram_drive; return (report, secs)."""
    body = {
        "project_id": project_id,
        "question": question,
        "output_format": "json",
        "depth": "standard",
        "include_insights": bool(include_insights),
    }
    t0 = time.time()
    proc = subprocess.run(
        [sys.executable, str(DRIVE), "tool", "ask_codebase", json.dumps(body), "400000"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        cwd=str(REPO),
    )
    dt = time.time() - t0
    out = proc.stdout.strip()
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


def item_text(e: dict) -> str:
    """ONE item's searchable text: its own path + its own content. Never a
    cross-item concatenation (doc 11 P0-2)."""
    return (str(e.get("path", "")) + " " + str(e.get("content", ""))).lower()


def is_extension_token(t: str) -> bool:
    return t.startswith(".")


def item_satisfies(e: dict, pred: dict) -> bool:
    """One evidence item vs one required_items predicate."""
    p = str(e.get("path", "")).lower()
    if "path_suffix" in pred and not p.endswith(str(pred["path_suffix"]).lower()):
        return False
    if "path_any" in pred and not any(path_matches(p, t) for t in pred["path_any"]):
        return False
    if "kind" in pred and str(e.get("kind", "")).lower() != str(pred["kind"]).lower():
        return False
    txt = item_text(e)
    return all(str(t).lower() in txt for t in pred.get("content_all", []))


def judge(row: dict, status: str, report: dict) -> tuple[bool, str]:
    """Doc 11 P0-2: is this row CORRECT at the level of individual evidence
    items — not merely status-labelled with the right tokens somewhere?"""
    expect = row["expect_status"]
    if status not in expect:
        return False, f"status {status} not in {expect}"
    if row.get("must_abstain"):
        return (True, "") if status == "unsupported" else (False, "did not abstain")
    if status not in ("answered", "partial"):
        # An abstention on an answerable row is only correct when the row says
        # so deliberately — never because expect_status was written broadly.
        if row.get("allow_abstain"):
            return True, ""
        return False, f"abstained ({status}) on an answerable row"
    ev = report.get("evidence", [])
    paths = [str(e.get("path", "")) for e in ev]
    if not ev:
        return False, "answered with zero evidence"

    mod = row.get("required_modality", [])
    if mod and not any(path_matches(p, t) for p in paths for t in mod):
        return False, f"no evidence of the requested modality {mod}"

    # Item-scoped required_all: each token must live inside ONE item.
    missing = [
        t
        for t in row.get("required_all", [])
        if not any(str(t).lower() in item_text(e) for e in ev)
    ]
    if missing:
        return False, f"required symbols/files not cited: {missing}"

    # Item-level predicates: path AND content in the SAME item.
    for pred in row.get("required_items", []):
        if not any(item_satisfies(e, pred) for e in ev):
            return False, f"no evidence item satisfies {pred}"

    # File-set recall for "which files" questions.
    answer_files = row.get("answer_files", [])
    if answer_files:
        hit = sum(
            1
            for f in answer_files
            if any(p.lower().endswith(str(f).lower()) for p in paths)
        )
        recall = hit / len(answer_files)
        need = row.get("min_file_recall", 1.0)
        if recall < need:
            return False, f"file-set recall {recall:.2f} < {need} ({hit}/{len(answer_files)})"

    bad = [p for p in paths if any(path_matches(p, t) for t in row.get("forbidden", []))]
    if bad:
        return False, f"forbidden distractor cited: {bad[:3]}"
    badc = [
        p
        for p in paths
        if any(str(c).lower() in p.lower() for c in row.get("forbidden_classes", []))
    ]
    if badc:
        return False, f"forbidden evidence class cited: {badc[:3]}"

    # Item-level precision; extension tokens prove nothing (doc 11 P0-2).
    toks = [
        t
        for t in (
            set(row.get("required_all", []))
            | set(row.get("must_cite_any", []))
            | {p.get("path_suffix", "") for p in row.get("required_items", [])}
            | {c for p in row.get("required_items", []) for c in p.get("content_all", [])}
            | set(answer_files)
        )
        if t and not is_extension_token(str(t))
    ]
    mp = row.get("min_precision", DEFAULT_MIN_PRECISION if toks else None)
    if mp is not None and toks:
        rel = sum(1 for e in ev if any(str(t).lower() in item_text(e) for t in toks))
        prec = rel / len(ev)
        if prec < mp:
            return False, f"item precision {prec:.2f} < {mp} ({rel}/{len(ev)} relevant)"
    return True, ""


def cite_coverage(row: dict, report: dict) -> float | None:
    cites = row.get("must_cite_any", [])
    if not cites:
        return None
    ev = report.get("evidence", [])
    hit = sum(
        1 for c in cites if any(str(c).lower() in item_text(e) for e in ev)
    )
    return hit / len(cites)


def main() -> int:
    if len(sys.argv) < 2:
        print(
            "usage: python eval/ask_engine_golden.py <project_id> [corpus.jsonl] [--insights] [--out record.json]"
        )
        return 2
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    out_path = None
    if "--out" in sys.argv:
        out_path = sys.argv[sys.argv.index("--out") + 1]
        args = [a for a in args if a != out_path]
    include_insights = "--insights" in sys.argv
    project_id = args[0]
    corpus_path = Path(args[1]) if len(args) > 1 else CORPUS
    rows = [
        json.loads(ln)
        for ln in corpus_path.read_text(encoding="utf-8").splitlines()
        if ln.strip()
    ]

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
        n_ins = sum(
            1
            for e in report.get("evidence", [])
            if e.get("authority") == "DreamerInsight"
        )
        insight_items += n_ins
        insight_rows += 1 if n_ins else 0
        status = report.get("status", "ERROR")
        expect = row["expect_status"]
        if status in expect:
            status_matches += 1
        if row.get("must_abstain"):
            abstain_total += 1
            if status == "unsupported":
                abstain_ok += 1

        cov = cite_coverage(row, report)
        if cov is not None:
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
                "evidence_paths": [
                    str(e.get("path", "")) for e in report.get("evidence", [])
                ][:12],
            }
        )
        exp_s = "/".join(expect)
        print(
            f"{row['id']:<18}{row['category']:<16}{status:<13}{exp_s:<20}{cite_s:<6}{int(dt * 1000):>6}  "
            f"{'ok' if ok else 'FAIL: ' + reason}"
        )

    n = len(rows)
    print("-" * 74)
    status_rate = status_matches / n if n else 0.0
    abstain_rate = (abstain_ok / abstain_total) if abstain_total else 1.0
    mean_cite = (sum(cite_scores) / len(cite_scores)) if cite_scores else 0.0
    print(f"judge:        {JUDGE_VERSION}")
    print(f"status-match: {status_matches}/{n} = {status_rate:.0%}   (gate >= 80%)")
    print(f"abstain:      {abstain_ok}/{abstain_total} = {abstain_rate:.0%}   (gate = 100%)")
    print(f"mean citation coverage (answerable rows): {mean_cite:.2f}")
    print(f"mean latency: {int(total_latency / n * 1000)} ms")
    print(
        f"dreamer insights: {'ON' if include_insights else 'OFF'}; insight evidence in {insight_rows}/{n} rows ({insight_items} items)"
    )
    print(
        f"correct:      {correct}/{n} = {correct / n if n else 0:.0%}   (gate = 100%: status + modality + item-scoped symbols + required_items + file recall + no forbidden + item precision)"
    )
    for fid, reason in failures:
        print(f"  FAIL {fid}: {reason}")

    gate_ok = abstain_rate >= 1.0 and status_rate >= 0.80 and correct == n
    print(f"\nGATE: {'PASS' if gate_ok else 'FAIL'}")
    if out_path:
        summary = {
            "judge": JUDGE_VERSION,
            "corpus": str(corpus_path),
            "corpus_sha256": hashlib.sha256(corpus_path.read_bytes()).hexdigest(),
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
